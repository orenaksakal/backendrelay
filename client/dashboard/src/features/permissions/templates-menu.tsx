import { ChevronDown } from 'lucide-react';
import { useState } from 'react';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import {
  PERMISSION_TEMPLATES,
  applyTemplate,
  type TemplateId,
} from '@/lib/permissions/document';
import type { Rules } from '@/lib/permissions/model';

export interface TemplatesMenuProps {
  namespace: string;
  rules: Rules;
  disabled?: boolean;
  onChange: (rules: Rules) => void;
}

/** C.10 template menu: inserts a whole action set after a confirmation. */
export function TemplatesMenu({
  namespace,
  rules,
  disabled,
  onChange,
}: TemplatesMenuProps) {
  const [pending, setPending] = useState<TemplateId | null>(null);
  const template = PERMISSION_TEMPLATES.find(
    (candidate) => candidate.id === pending,
  );

  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger
          render={
            <Button
              type="button"
              variant="secondary"
              size="sm"
              disabled={disabled}
            />
          }
        >
          Templates
          <ChevronDown className="size-4" aria-hidden="true" />
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end">
          <DropdownMenuGroup>
            <DropdownMenuLabel>Replace this namespace</DropdownMenuLabel>
            {PERMISSION_TEMPLATES.map((candidate) => (
              <DropdownMenuItem
                key={candidate.id}
                onClick={() => {
                  setPending(candidate.id);
                }}
              >
                {candidate.label}
              </DropdownMenuItem>
            ))}
          </DropdownMenuGroup>
        </DropdownMenuContent>
      </DropdownMenu>

      <Dialog
        open={pending !== null}
        onOpenChange={(open) => {
          if (!open) setPending(null);
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Replace rules for {namespace}?</DialogTitle>
            <DialogDescription>
              {template?.description} This replaces the namespace&apos;s view,
              create, update, delete, link and unlink rules. Field and bind
              rules stay.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              type="button"
              variant="secondary"
              onClick={() => {
                setPending(null);
              }}
            >
              Cancel
            </Button>
            <Button
              type="button"
              onClick={() => {
                if (pending !== null) {
                  onChange(applyTemplate(rules, namespace, pending));
                }
                setPending(null);
              }}
            >
              Replace rules
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
