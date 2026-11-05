import { CopyButton } from '@/components/data/copy-button';
import { cn } from '@/lib/utils';

export interface CodeBlockProps {
  code: string;
  /** Shown in the header, for example `json` or `bash`. */
  language: string;
  className?: string;
}

/** Read-only snippet with a language label and a CopyButton (B.4). */
export function CodeBlock({ code, language, className }: CodeBlockProps) {
  return (
    <figure
      className={cn(
        'border-border bg-muted/50 overflow-hidden rounded-md border',
        className,
      )}
    >
      <figcaption className="border-border text-muted-foreground flex items-center justify-between gap-2 border-b px-3 py-1.5 text-xs">
        <span className="font-mono">{language}</span>
        <CopyButton value={code} aria-label={`Copy ${language} snippet`} />
      </figcaption>
      <pre className="text-cell overflow-x-auto p-3 font-mono">
        <code>{code}</code>
      </pre>
    </figure>
  );
}
