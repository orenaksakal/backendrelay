import { Link } from '@tanstack/react-router';
import { Diamond } from 'lucide-react';
import { useId, type ReactNode } from 'react';
import { cn } from '@/lib/utils';

export interface AuthCardProps {
  title: ReactNode;
  /** Optional logo or avatar shown above the title, inside the card. */
  icon?: ReactNode;
  description?: ReactNode;
  /** C.1 uses 400px; C.2 uses 480px (B.3). */
  width?: 'sm' | 'wide';
  children: ReactNode;
}

/**
 * The centred card shared by the signed-out identity screens (C.1, C.2). The
 * brand link sits above it, and `main` is the skip-link target.
 */
export function AuthCard({
  title,
  icon,
  description,
  width = 'sm',
  children,
}: AuthCardProps) {
  const titleId = useId();

  return (
    <main
      id="main"
      tabIndex={-1}
      className="bg-background flex min-h-dvh flex-col items-center justify-center p-4 md:p-6"
    >
      <div
        className={cn(
          'w-full',
          width === 'wide' ? 'max-w-[480px]' : 'max-w-[400px]',
        )}
      >
        <Link
          to="/"
          aria-label="BackendRelay home"
          className="text-foreground inline-flex h-8 items-center gap-2 rounded-md px-1.5 font-semibold"
        >
          <Diamond aria-hidden="true" className="size-4" />
          <span>BackendRelay</span>
        </Link>

        <section
          aria-labelledby={titleId}
          className="border-border bg-card mt-4 rounded-lg border p-6"
        >
          {icon ? (
            <div className="mb-3 flex items-center gap-3">{icon}</div>
          ) : null}
          <h1 id={titleId} className="text-page">
            {title}
          </h1>
          {description ? (
            <p className="text-muted-foreground mt-1 text-sm">{description}</p>
          ) : null}
          <div className="mt-6">{children}</div>
        </section>
      </div>
    </main>
  );
}
