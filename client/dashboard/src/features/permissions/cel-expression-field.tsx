import type { CompletionSource } from '@codemirror/autocomplete';
import { useState } from 'react';
import { CodeEditor } from '@/components/code-editor/code-editor';
import { lintCelExpression } from '@/lib/permissions/cel';

export interface CelExpressionFieldProps {
  value: string;
  onChange: (value: string) => void;
  /** Required: these rows have no visible label besides the action name. */
  'aria-label': string;
  completions?: CompletionSource;
  readOnly?: boolean;
  /** The server's (D0.5) message for this exact rule. */
  serverError?: string;
}

/**
 * One CEL expression row: a single-line editor that expands while focused, a
 * client-side syntax check, and the server's validation message underneath.
 * The server stays authoritative; this only keeps the footer useful.
 */
export function CelExpressionField({
  value,
  onChange,
  'aria-label': ariaLabel,
  completions,
  readOnly,
  serverError,
}: CelExpressionFieldProps) {
  const [focused, setFocused] = useState(false);

  return (
    <div className="flex min-w-0 flex-col gap-1">
      <div
        onFocusCapture={() => {
          setFocused(true);
        }}
        onBlurCapture={() => {
          setFocused(false);
        }}
      >
        <CodeEditor
          value={value}
          mode="cel-in-json"
          aria-label={ariaLabel}
          height={focused ? '84px' : '36px'}
          readOnly={readOnly}
          completions={completions}
          validate={lintCelExpression}
          onChange={onChange}
        />
      </div>
      {serverError !== undefined ? (
        <p role="alert" className="text-destructive text-xs">
          {serverError}
        </p>
      ) : null}
    </div>
  );
}
