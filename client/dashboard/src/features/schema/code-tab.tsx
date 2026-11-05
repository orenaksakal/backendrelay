import { Download } from 'lucide-react';
import { useMemo } from 'react';
import { CodeEditor } from '@/components/code-editor/code-editor';
import { CopyButton } from '@/components/data/copy-button';
import { Button } from '@/components/ui/button';
import type { InstantAttr } from '@/lib/instant';
import {
  SCHEMA_TYPESCRIPT_FILENAME,
  downloadSchemaTypescript,
  schemaTypescriptFile,
} from '@/lib/schema/code';

export interface CodeTabProps {
  attrs: Record<string, InstantAttr> | null;
}

/**
 * C.7 Code tab: the generated `instant.schema.ts`, read-only, with Copy and a
 * download. Nothing here edits the app; the file is derived from the catalog.
 */
export function CodeTab({ attrs }: CodeTabProps) {
  const code = useMemo(() => schemaTypescriptFile(attrs), [attrs]);

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-cell font-mono">{SCHEMA_TYPESCRIPT_FILENAME}</p>
        <div className="flex items-center gap-1">
          <CopyButton
            value={code}
            label="Copy"
            aria-label="Copy instant.schema.ts"
          />
          <Button
            type="button"
            variant="secondary"
            size="sm"
            disabled={attrs === null}
            onClick={() => {
              downloadSchemaTypescript(code);
            }}
          >
            <Download aria-hidden="true" />
            Download
          </Button>
        </div>
      </div>

      <CodeEditor
        value={code}
        mode="readonly-ts"
        aria-label="Generated instant.schema.ts"
        readOnly
        height="60vh"
      />
    </div>
  );
}
