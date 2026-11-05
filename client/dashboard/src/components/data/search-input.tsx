import { Search, X } from 'lucide-react';
import { useEffect, useRef, useState, type KeyboardEvent } from 'react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Kbd } from '@/components/ui/kbd';
import { cn } from '@/lib/utils';

export interface SearchInputProps {
  /** Controlled value from the URL or page state. */
  value: string;
  onValueChange: (value: string) => void;
  /** Required: the search box is often the only cue for what it searches. */
  'aria-label': string;
  placeholder?: string;
  /** Debounce before `onValueChange` fires. Esc and Clear bypass it. */
  debounceMs?: number;
  /** Show the `/` key hint (page-level searches only). */
  showShortcut?: boolean;
  className?: string;
}

/**
 * Search field with a magnifier, a clear button and the `/` hint. Esc clears
 * the text first, then blurs the field; it never reaches the enclosing sheet,
 * so an open sheet stays open (B.4).
 */
export function SearchInput({
  value,
  onValueChange,
  'aria-label': ariaLabel,
  placeholder,
  debounceMs = 200,
  showShortcut = false,
  className,
}: SearchInputProps) {
  const inputRef = useRef<HTMLInputElement>(null);
  // While typing, the local draft wins; afterwards the controlled value does.
  const [draft, setDraft] = useState<string | null>(null);
  const onValueChangeRef = useRef(onValueChange);

  useEffect(() => {
    onValueChangeRef.current = onValueChange;
  }, [onValueChange]);

  useEffect(() => {
    if (draft === null) {
      return;
    }
    const timer = setTimeout(() => {
      onValueChangeRef.current(draft);
      setDraft(null);
    }, debounceMs);
    return () => {
      clearTimeout(timer);
    };
  }, [draft, debounceMs]);

  const shown = draft ?? value;

  function emit(next: string) {
    onValueChangeRef.current(next);
    setDraft(null);
  }

  function handleKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (event.key !== 'Escape') {
      return;
    }
    // Keep the sheet or dialog around this field open; Esc is ours first.
    event.preventDefault();
    event.stopPropagation();

    if (shown !== '') {
      emit('');
      return;
    }
    inputRef.current?.blur();
  }

  return (
    <div className={cn('relative flex items-center', className)}>
      <Search
        className="text-muted-foreground pointer-events-none absolute left-2.5 size-4"
        aria-hidden="true"
      />
      <Input
        ref={inputRef}
        type="text"
        role="searchbox"
        value={shown}
        placeholder={placeholder}
        aria-label={ariaLabel}
        className="pr-8 pl-8"
        onChange={(event) => {
          setDraft(event.target.value);
        }}
        onKeyDown={handleKeyDown}
      />
      {shown !== '' ? (
        <Button
          type="button"
          variant="ghost"
          size="sm"
          aria-label="Clear search"
          className="absolute right-0.5 size-7 p-0"
          onClick={() => {
            emit('');
          }}
        >
          <X className="size-4" aria-hidden="true" />
        </Button>
      ) : showShortcut ? (
        <Kbd className="absolute right-2">/</Kbd>
      ) : null}
    </div>
  );
}
