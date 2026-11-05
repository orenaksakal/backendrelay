import {
  CodeEditor,
  type JsonSchemaDocument,
} from '@/components/code-editor/code-editor';
import { CopyButton } from '@/components/data/copy-button';

export interface PermissionsJsonViewProps {
  value: string;
  schema: JsonSchemaDocument;
  onChange: (value: string) => void;
}

/**
 * C.10 JSON view: the shared draft in `json-with-schema` mode, so lint,
 * completion and hover all describe the permission document.
 */
export function PermissionsJsonView({
  value,
  schema,
  onChange,
}: PermissionsJsonViewProps) {
  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-cell font-mono">rules.json</p>
        <CopyButton value={value} label="Copy" aria-label="Copy rules.json" />
      </div>
      <CodeEditor
        value={value}
        mode="json-with-schema"
        schema={schema}
        aria-label="Permission rules JSON"
        height="60vh"
        onChange={onChange}
      />
      <p className="text-muted-foreground text-xs">
        CEL expressions are strings. Changes stay in the browser until you save.
      </p>
    </div>
  );
}
