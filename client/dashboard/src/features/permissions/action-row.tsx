import { Copy, Lock, LockOpen, MoreHorizontal, RotateCcw } from 'lucide-react';
import { useId } from 'react';
import { IconButton } from '@/components/layout/icon-button';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import type { AppSchema } from '@/lib/instant';
import { copyText } from '@/lib/clipboard';
import { celCompletionSource } from '@/lib/permissions/cel';
import { celContextFor } from '@/lib/permissions/editor-context';
import { ownExpression, setAllowExpression } from '@/lib/permissions/document';
import {
  PERMISSION_ACTION_LABELS,
  deniesEveryone,
  describeEffectiveRule,
  effectiveRule,
  type PermissionAction,
} from '@/lib/permissions/effective';
import type { Rules } from '@/lib/permissions/model';
import { toastManager } from '@/lib/toast';
import { cn } from '@/lib/utils';
import { CelExpressionField } from './cel-expression-field';

export interface ActionRowProps {
  namespace: string;
  action: PermissionAction;
  rules: Rules;
  schema: AppSchema | undefined;
  readOnly: boolean;
  serverError?: string;
  /** Set when the Overview opened the Editor on this exact cell. */
  highlighted?: boolean;
  onChange: (rules: Rules) => void;
  onToggleLock: (previous: string | null) => void;
}

/** One row per object action (C.10): expression, effective chip, lock and menu. */
export function ActionRow({
  namespace,
  action,
  rules,
  schema,
  readOnly,
  serverError,
  highlighted,
  onChange,
  onToggleLock,
}: ActionRowProps) {
  const descriptionId = useId();
  const rule = effectiveRule(rules, namespace, action);
  const view = describeEffectiveRule(rule, namespace);
  const label = PERMISSION_ACTION_LABELS[action];
  const own = ownExpression(rules, namespace, action);
  const locked = rule.own && deniesEveryone(rule.expression);
  const completions = celCompletionSource(
    celContextFor(schema, rules, namespace, action),
  );

  return (
    <div
      className={cn(
        'border-border flex flex-col gap-2 border-b py-2 last:border-b-0 md:flex-row md:items-start md:gap-3',
        highlighted && 'bg-accent/12 -mx-2 rounded-md px-2',
      )}
    >
      <span className="text-cell shrink-0 font-medium md:w-16 md:pt-2">
        {label}
      </span>
      <div className="min-w-0 flex-1">
        <CelExpressionField
          value={own ?? ''}
          aria-label={`${namespace} ${action} rule`}
          completions={completions}
          readOnly={readOnly}
          serverError={serverError}
          onChange={(value) => {
            onChange(
              setAllowExpression(
                rules,
                namespace,
                action,
                value.trim() === '' ? null : value,
              ),
            );
          }}
        />
        {own === null && rule.source === 'namespace-default' ? (
          <p className="text-muted-foreground mt-1 text-xs">
            Inherits {rule.expression} from this namespace&apos;s
            allow.$default.
          </p>
        ) : own === null && rule.source === 'app-action' ? (
          <p className="text-muted-foreground mt-1 text-xs">
            Inherits {rule.expression} from $default.allow.{action}.
          </p>
        ) : own === null && rule.source === 'app-default' ? (
          <p className="text-muted-foreground mt-1 text-xs">
            Inherits {rule.expression} from $default.allow.$default.
          </p>
        ) : null}
      </div>
      <div className="flex shrink-0 items-center gap-1 md:pt-1">
        <Badge
          tone={view.tone}
          title={view.description}
          aria-describedby={descriptionId}
        >
          {view.label}
        </Badge>
        <span id={descriptionId} className="sr-only">
          Effective rule: {view.description}
        </span>
        <IconButton
          aria-label={
            locked
              ? `Unlock ${action} rule`
              : `Lock ${action} rule (admin only)`
          }
          icon={
            locked ? (
              <LockOpen className="size-4" aria-hidden="true" />
            ) : (
              <Lock className="size-4" aria-hidden="true" />
            )
          }
          disabled={readOnly}
          onClick={() => {
            onToggleLock(own);
          }}
        />
        <DropdownMenu>
          <DropdownMenuTrigger
            render={
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="size-8 p-0"
                aria-label={`${label} rule actions`}
                disabled={readOnly}
              />
            }
          >
            <MoreHorizontal className="size-4" aria-hidden="true" />
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem
              disabled={own === null}
              onClick={() => {
                onChange(setAllowExpression(rules, namespace, action, null));
              }}
            >
              <RotateCcw aria-hidden="true" />
              Reset to inherited
            </DropdownMenuItem>
            <DropdownMenuSeparator />
            <DropdownMenuItem
              onClick={() => {
                void copyText(rule.expression).then((copied) => {
                  toastManager.add({
                    type: copied ? 'success' : 'error',
                    title: copied
                      ? 'Expression copied'
                      : "Couldn't copy the expression",
                  });
                });
              }}
            >
              <Copy aria-hidden="true" />
              Copy expression
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </div>
    </div>
  );
}
