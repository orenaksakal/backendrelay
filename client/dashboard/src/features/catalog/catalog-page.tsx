import type { ColDef } from 'ag-grid-community';
import {
  Boxes,
  Download,
  Loader2,
  MoreHorizontal,
  Plus,
  RefreshCw,
  Star,
  Trash2,
  Users,
} from 'lucide-react';
import { useEffect, useId, useRef, useState, type ReactNode } from 'react';
import { Chart, type ChartRow } from '@/components/charts/chart';
import { CodeBlock } from '@/components/code-editor/code-block';
import { CodeEditor } from '@/components/code-editor/code-editor';
import { CopyButton } from '@/components/data/copy-button';
import { FilterButton, FilterPanel } from '@/components/data/filter-panel';
import {
  createFilterCondition,
  type FilterAttribute,
  type FilterCondition,
} from '@/components/data/filter-serialization';
import { Bytes, Count } from '@/components/data/formatters';
import { IdChip } from '@/components/data/id-chip';
import { KeyValueList } from '@/components/data/key-value-list';
import { Pagination } from '@/components/data/pagination';
import { RelativeTime } from '@/components/data/relative-time';
import { SearchInput } from '@/components/data/search-input';
import { SecretField } from '@/components/data/secret-field';
import {
  SimpleTable,
  type SimpleTableColumn,
} from '@/components/data/simple-table';
import DataGrid from '@/components/data-grid/data-grid';
import { ConfirmDialog } from '@/components/feedback/confirm-dialog';
import { EmptyState } from '@/components/feedback/empty-state';
import { ErrorState } from '@/components/feedback/error-state';
import {
  CardSkeleton,
  FormSkeleton,
  TableSkeleton,
} from '@/components/feedback/skeletons';
import { StatusPill } from '@/components/feedback/status-pill';
import { TypedConfirmDialog } from '@/components/feedback/typed-confirm-dialog';
import { IconButton } from '@/components/layout/icon-button';
import { PageHeader } from '@/components/layout/page-header';
import { SectionCard } from '@/components/layout/section-card';
import { SettingsRow } from '@/components/layout/settings-row';
import { UnsavedChangesBar } from '@/components/layout/unsaved-changes-bar';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import {
  Combobox,
  ComboboxContent,
  ComboboxInput,
  ComboboxInputGroup,
  ComboboxItem,
  ComboboxList,
  ComboboxTrigger,
} from '@/components/ui/combobox';
import {
  Command,
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandShortcut,
} from '@/components/ui/command';
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuGroup,
  ContextMenuItem,
  ContextMenuLabel,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from '@/components/ui/context-menu';
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from '@/components/ui/dialog';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import {
  Field,
  FieldDescription,
  FieldError,
  FieldLabel,
} from '@/components/ui/field';
import { Input } from '@/components/ui/input';
import { Kbd, KbdGroup } from '@/components/ui/kbd';
import {
  Popover,
  PopoverClose,
  PopoverContent,
  PopoverDescription,
  PopoverTitle,
  PopoverTrigger,
} from '@/components/ui/popover';
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  Sheet,
  SheetBody,
  SheetClose,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
  SheetTrigger,
} from '@/components/ui/sheet';
import { Switch } from '@/components/ui/switch';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { Textarea } from '@/components/ui/textarea';
import { Toaster, ToastProvider, useToast } from '@/components/ui/toast';
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from '@/components/ui/tooltip';
import { useTheme } from '@/lib/theme';

const SECTIONS = [
  { id: 'button', title: 'Button' },
  { id: 'icon-button', title: 'IconButton' },
  { id: 'input', title: 'Input' },
  { id: 'textarea', title: 'Textarea' },
  { id: 'select', title: 'Select' },
  { id: 'combobox', title: 'Combobox' },
  { id: 'checkbox', title: 'Checkbox' },
  { id: 'switch', title: 'Switch' },
  { id: 'radio-group', title: 'RadioGroup' },
  { id: 'tabs', title: 'Tabs' },
  { id: 'badge', title: 'Badge' },
  { id: 'status-pill', title: 'StatusPill' },
  { id: 'tooltip', title: 'Tooltip' },
  { id: 'popover', title: 'Popover' },
  { id: 'dropdown-menu', title: 'DropdownMenu' },
  { id: 'context-menu', title: 'ContextMenu' },
  { id: 'dialog', title: 'Dialog' },
  { id: 'sheet', title: 'Sheet' },
  { id: 'confirm-dialog', title: 'ConfirmDialog' },
  { id: 'typed-confirm-dialog', title: 'TypedConfirmDialog' },
  { id: 'toast', title: 'Toast' },
  { id: 'page-header', title: 'PageHeader' },
  { id: 'section-card', title: 'SectionCard' },
  { id: 'settings-row', title: 'SettingsRow' },
  { id: 'unsaved-changes-bar', title: 'UnsavedChangesBar' },
  { id: 'empty-state', title: 'EmptyState' },
  { id: 'error-state', title: 'ErrorState' },
  { id: 'skeletons', title: 'Skeletons' },
  { id: 'id-chip', title: 'IdChip' },
  { id: 'copy-button', title: 'CopyButton' },
  { id: 'secret-field', title: 'SecretField' },
  { id: 'key-value-list', title: 'KeyValueList' },
  { id: 'relative-time', title: 'RelativeTime' },
  { id: 'formatters', title: 'Bytes and Count' },
  { id: 'search-input', title: 'SearchInput' },
  { id: 'filter', title: 'FilterButton and FilterPanel' },
  { id: 'pagination', title: 'Pagination' },
  { id: 'simple-table', title: 'SimpleTable' },
  { id: 'data-grid', title: 'DataGrid' },
  { id: 'code-editor', title: 'CodeEditor' },
  { id: 'code-block', title: 'CodeBlock' },
  { id: 'kbd', title: 'Kbd' },
  { id: 'chart', title: 'Chart' },
  { id: 'command-palette', title: 'CommandPalette' },
] as const;

const noop = () => undefined;

const ORG_ROLES = [
  { value: 'owner', label: 'Owner' },
  { value: 'admin', label: 'Admin' },
  { value: 'member', label: 'Member' },
] as const;

const FILTER_ATTRIBUTES: FilterAttribute[] = [
  { name: 'status', type: 'string' },
  { name: 'email', type: 'string' },
  { name: 'createdAt', type: 'date', indexed: true },
  { name: 'visits', type: 'number', indexed: true },
];

interface DemoTableRow {
  id: string;
  name: string;
  email: string;
  status: 'Active' | 'Paused';
}

const TABLE_ROWS: DemoTableRow[] = [
  { id: '1', name: 'Ada Lovelace', email: 'ada@example.com', status: 'Active' },
  {
    id: '2',
    name: 'Grace Hopper',
    email: 'grace@example.com',
    status: 'Active',
  },
  { id: '3', name: 'Alan Turing', email: 'alan@example.com', status: 'Paused' },
];

const TABLE_COLUMNS: SimpleTableColumn<DemoTableRow>[] = [
  {
    id: 'name',
    header: 'Name',
    sortable: true,
    cell: (row) => <span className="font-medium">{row.name}</span>,
  },
  { id: 'email', header: 'Email', cell: (row) => row.email },
  {
    id: 'status',
    header: 'Status',
    align: 'right',
    cell: (row) => (
      <StatusPill status={row.status === 'Active' ? 'active' : 'read-only'} />
    ),
  },
];

interface GridRow {
  id: string;
  namespace: string;
  entity: string;
  updated: string;
}

const GRID_COLUMNS: ColDef<GridRow>[] = [
  { field: 'entity', headerName: 'Entity', flex: 1 },
  { field: 'namespace', headerName: 'Namespace', width: 130 },
  { field: 'updated', headerName: 'Updated', width: 150 },
];

const GRID_ROWS: GridRow[] = [
  { id: 'e1', namespace: 'todos', entity: 'todo-1', updated: '3 min ago' },
  { id: 'e2', namespace: 'todos', entity: 'todo-2', updated: '8 min ago' },
  { id: 'e3', namespace: 'users', entity: 'user-1', updated: '1 hour ago' },
];

const CHART_DATA: ChartRow[] = [
  { day: 'Mon', sessions: 12 },
  { day: 'Tue', sessions: 18 },
  { day: 'Wed', sessions: 9 },
  { day: 'Thu', sessions: 21 },
  { day: 'Fri', sessions: 16 },
];

const JSON_SAMPLE = `{
  "namespace": "todos",
  "attrs": {
    "title": "string",
    "completed": "boolean"
  }
}`;

const INVALID_JSON = `{
  "namespace": "todos",
  "attrs": {
    "title": "string",
}`;

const TYPESCRIPT_SAMPLE = `type Todo = {
  id: string;
  title: string;
  completed: boolean;
};

const todo: Todo = { id: 't1', title: 'Ship it', completed: false };`;

export function CatalogPage() {
  return (
    <ToastProvider>
      <TooltipProvider>
        <div className="bg-background text-foreground min-h-screen">
          <header className="border-border bg-sidebar sticky top-0 z-(--z-sticky) border-b">
            <div className="mx-auto flex max-w-[1600px] flex-wrap items-center gap-3 px-4 py-2">
              <div className="flex min-w-0 flex-col">
                <h1 id="catalog-title" className="text-page">
                  Component catalog
                </h1>
                <p className="text-muted-foreground text-xs">
                  Development only. Every design-system component, in every
                  state, in the current theme.
                </p>
              </div>
              <div className="ml-auto">
                <ThemeControl />
              </div>
            </div>
          </header>

          <div className="mx-auto flex max-w-[1600px] gap-6 px-4 py-6">
            <nav
              aria-label="Catalog sections"
              className="hidden w-56 shrink-0 lg:block"
            >
              <ol className="sticky top-16 flex max-h-[calc(100vh-5rem)] flex-col gap-0.5 overflow-y-auto pr-2">
                {SECTIONS.map((section) => (
                  <li key={section.id}>
                    <a
                      href={`#${section.id}`}
                      className="text-muted-foreground hover:bg-muted hover:text-foreground block rounded-md px-2 py-1 text-sm"
                    >
                      {section.title}
                    </a>
                  </li>
                ))}
              </ol>
            </nav>

            <main
              id="main"
              tabIndex={-1}
              aria-labelledby="catalog-title"
              className="flex min-w-0 flex-1 flex-col gap-6 outline-none"
            >
              <ButtonSection />
              <IconButtonSection />
              <InputSection />
              <TextareaSection />
              <SelectSection />
              <ComboboxSection />
              <CheckboxSection />
              <SwitchSection />
              <RadioGroupSection />
              <TabsSection />
              <BadgeSection />
              <StatusPillSection />
              <TooltipSection />
              <PopoverSection />
              <DropdownMenuSection />
              <ContextMenuSection />
              <DialogSection />
              <SheetSection />
              <ConfirmDialogSection />
              <TypedConfirmDialogSection />
              <ToastSection />
              <PageHeaderSection />
              <SectionCardSection />
              <SettingsRowSection />
              <UnsavedChangesBarSection />
              <EmptyStateSection />
              <ErrorStateSection />
              <SkeletonsSection />
              <IdChipSection />
              <CopyButtonSection />
              <SecretFieldSection />
              <KeyValueListSection />
              <RelativeTimeSection />
              <FormattersSection />
              <SearchInputSection />
              <FilterSection />
              <PaginationSection />
              <SimpleTableSection />
              <DataGridSection />
              <CodeEditorSection />
              <CodeBlockSection />
              <KbdSection />
              <ChartSection />
              <CommandPaletteSection />
            </main>
          </div>

          <Toaster />
        </div>
      </TooltipProvider>
    </ToastProvider>
  );
}

function ThemeControl() {
  const { theme, resolvedTheme, setTheme } = useTheme();

  return (
    <div
      role="group"
      aria-label="Theme"
      className="border-border flex items-center gap-1 rounded-md border p-0.5"
    >
      {(['light', 'dark', 'system'] as const).map((option) => (
        <Button
          key={option}
          type="button"
          size="sm"
          variant={theme === option ? 'secondary' : 'ghost'}
          aria-pressed={theme === option}
          onClick={() => {
            setTheme(option);
          }}
        >
          {option === 'light' ? 'Light' : option === 'dark' ? 'Dark' : 'System'}
        </Button>
      ))}
      <span className="text-muted-foreground px-1.5 text-xs">
        {resolvedTheme}
      </span>
    </div>
  );
}

interface CatalogSectionProps {
  id: string;
  title: string;
  description?: string;
  children: ReactNode;
}

function CatalogSection({
  id,
  title,
  description,
  children,
}: CatalogSectionProps) {
  return (
    <section
      id={id}
      aria-labelledby={`${id}-title`}
      className="border-border bg-card scroll-mt-16 rounded-lg border"
    >
      <header className="border-border border-b p-4">
        <h2 id={`${id}-title`} className="text-section">
          {title}
        </h2>
        {description ? (
          <p className="text-muted-foreground mt-0.5 text-xs">{description}</p>
        ) : null}
      </header>
      <div className="flex flex-col gap-5 p-4">{children}</div>
    </section>
  );
}

function State({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-2">
      <h3 className="text-muted-foreground text-xs font-medium">{title}</h3>
      <div className="flex flex-wrap items-start gap-3">{children}</div>
    </div>
  );
}

interface FieldControlState {
  id: string;
  describedBy: string | undefined;
  invalid: boolean;
}

interface FieldExampleProps {
  label: string;
  description?: string;
  error?: string;
  disabled?: boolean;
  children: (state: FieldControlState) => ReactNode;
}

function FieldExample({
  label,
  description,
  error,
  disabled,
  children,
}: FieldExampleProps) {
  const id = useId();
  const descriptionId = `${id}-description`;
  const errorId = `${id}-error`;
  const describedBy = [
    description ? descriptionId : null,
    error ? errorId : null,
  ]
    .filter((value): value is string => value !== null)
    .join(' ');

  return (
    <Field disabled={disabled} invalid={Boolean(error)}>
      <FieldLabel htmlFor={id}>{label}</FieldLabel>
      {children({
        id,
        describedBy: describedBy || undefined,
        invalid: Boolean(error),
      })}
      {description ? (
        <FieldDescription id={descriptionId}>{description}</FieldDescription>
      ) : null}
      {error ? (
        <FieldError id={errorId} match>
          {error}
        </FieldError>
      ) : null}
    </Field>
  );
}

function ButtonSection() {
  const variants = [
    'primary',
    'secondary',
    'ghost',
    'destructive',
    'link',
  ] as const;
  const sizes = ['sm', 'md', 'lg'] as const;

  return (
    <CatalogSection
      id="button"
      title="Button"
      description="Every variant and size, plus icon slots, loading and disabled-with-reason."
    >
      {sizes.map((size) => (
        <State key={size} title={`Variant × size ${size}`}>
          {variants.map((variant) => (
            <Button key={variant} variant={variant} size={size}>
              {variant === 'primary'
                ? 'Create app'
                : variant === 'secondary'
                  ? 'Cancel'
                  : variant === 'ghost'
                    ? 'Dismiss'
                    : variant === 'destructive'
                      ? 'Delete app'
                      : 'Learn more'}
            </Button>
          ))}
        </State>
      ))}
      <State title="Leading icon and trailing shortcut">
        <Button>
          <Plus aria-hidden="true" />
          Create app
          <Kbd>⌘N</Kbd>
        </Button>
        <Button variant="secondary">
          <Download aria-hidden="true" />
          Export
        </Button>
      </State>
      <State title="Loading (keeps its width)">
        <Button aria-busy="true" className="min-w-32" disabled>
          <Loader2 className="animate-spin" aria-hidden="true" />
          Creating app…
        </Button>
        <p className="text-muted-foreground max-w-80 text-xs">
          The current Button does not expose a `loading` prop yet; this uses the
          same visual treatment so the state can be reviewed.
        </p>
      </State>
      <State title="Disabled with a tooltip reason">
        <Tooltip>
          <TooltipTrigger
            render={
              <Button aria-disabled="true" className="opacity-50">
                Delete app
              </Button>
            }
          />
          <TooltipContent>Only the owner can delete this app.</TooltipContent>
        </Tooltip>
      </State>
    </CatalogSection>
  );
}

function IconButtonSection() {
  return (
    <CatalogSection
      id="icon-button"
      title="IconButton"
      description="Square 28 or 32px; the accessible label is required and repeated in the tooltip."
    >
      <State title="Default">
        <IconButton aria-label="Refresh" icon={<RefreshCw />} shortcut="R" />
        <IconButton
          aria-label="More actions"
          icon={<MoreHorizontal />}
          size="sm"
        />
      </State>
      <State title="Active">
        <IconButton aria-label="Starred" icon={<Star />} variant="secondary" />
      </State>
      <State title="Disabled">
        <IconButton
          aria-label="Refresh disabled"
          icon={<RefreshCw />}
          disabled
        />
      </State>
    </CatalogSection>
  );
}

function InputSection() {
  return (
    <CatalogSection id="input" title="Input">
      <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
        <FieldExample label="Empty" description="Shown in the app switcher.">
          {({ id, describedBy, invalid }) => (
            <Input
              id={id}
              aria-describedby={describedBy}
              aria-invalid={invalid || undefined}
              placeholder="todo-app"
            />
          )}
        </FieldExample>
        <FieldExample label="Filled">
          {({ id, describedBy }) => (
            <Input
              id={id}
              aria-describedby={describedBy}
              defaultValue="todo-app"
            />
          )}
        </FieldExample>
        <FieldExample label="Error" error="Use at least 3 characters.">
          {({ id, describedBy, invalid }) => (
            <Input
              id={id}
              aria-describedby={describedBy}
              aria-invalid={invalid || undefined}
              defaultValue="x"
            />
          )}
        </FieldExample>
        <FieldExample label="Disabled" disabled>
          {({ id, describedBy }) => (
            <Input
              id={id}
              aria-describedby={describedBy}
              defaultValue="managed-by-org"
              disabled
            />
          )}
        </FieldExample>
        <FieldExample
          label="Focus"
          description="Tab here to see the focus ring."
        >
          {({ id, describedBy }) => (
            <Input
              id={id}
              aria-describedby={describedBy}
              placeholder="Focus me with Tab"
            />
          )}
        </FieldExample>
      </div>
    </CatalogSection>
  );
}

function TextareaSection() {
  return (
    <CatalogSection id="textarea" title="Textarea">
      <div className="grid gap-4 md:grid-cols-2">
        <FieldExample label="Empty" description="Markdown is supported.">
          {({ id, describedBy }) => (
            <Textarea
              id={id}
              aria-describedby={describedBy}
              placeholder="Describe the webhook payload…"
            />
          )}
        </FieldExample>
        <FieldExample label="Filled">
          {({ id, describedBy }) => (
            <Textarea
              id={id}
              aria-describedby={describedBy}
              defaultValue="Send a POST request when a todo is created."
            />
          )}
        </FieldExample>
        <FieldExample label="Error" error="Keep it under 500 characters.">
          {({ id, describedBy, invalid }) => (
            <Textarea
              id={id}
              aria-describedby={describedBy}
              aria-invalid={invalid || undefined}
              defaultValue="Too long…"
            />
          )}
        </FieldExample>
        <FieldExample label="Disabled" disabled>
          {({ id, describedBy }) => (
            <Textarea
              id={id}
              aria-describedby={describedBy}
              defaultValue="Managed by the operator."
              disabled
            />
          )}
        </FieldExample>
      </div>
    </CatalogSection>
  );
}

function SelectField({
  label,
  state,
}: {
  label: string;
  state: 'empty' | 'filled' | 'error' | 'disabled';
}) {
  const isFilled = state === 'filled';
  const isDisabled = state === 'disabled';
  const isError = state === 'error';

  return (
    <FieldExample
      label={label}
      error={isError ? 'Choose a role before saving.' : undefined}
      disabled={isDisabled}
    >
      {({ id, describedBy, invalid }) => (
        <Select
          defaultValue={isFilled ? 'admin' : undefined}
          disabled={isDisabled}
        >
          <SelectTrigger
            id={id}
            aria-describedby={describedBy}
            aria-invalid={invalid || undefined}
            disabled={isDisabled}
          >
            <SelectValue placeholder="Select a role" />
          </SelectTrigger>
          <SelectContent>
            {ORG_ROLES.map((role) => (
              <SelectItem key={role.value} value={role.value}>
                {role.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      )}
    </FieldExample>
  );
}

function SelectSection() {
  return (
    <CatalogSection
      id="select"
      title="Select"
      description="On/off settings use Switch, never a two-option select."
    >
      <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
        <SelectField label="Empty" state="empty" />
        <SelectField label="Filled" state="filled" />
        <SelectField label="Error" state="error" />
        <SelectField label="Disabled" state="disabled" />
      </div>
    </CatalogSection>
  );
}

function ComboboxField({
  label,
  state,
}: {
  label: string;
  state: 'empty' | 'filled' | 'error' | 'disabled';
}) {
  const isFilled = state === 'filled';
  const isDisabled = state === 'disabled';
  const isError = state === 'error';

  return (
    <FieldExample
      label={label}
      error={isError ? 'Pick an environment.' : undefined}
      disabled={isDisabled}
    >
      {({ id, describedBy, invalid }) => (
        <Combobox
          defaultValue={isFilled ? 'production' : undefined}
          disabled={isDisabled}
        >
          <ComboboxInputGroup>
            <ComboboxInput
              id={id}
              aria-describedby={describedBy}
              aria-invalid={invalid || undefined}
              placeholder="Search environments"
              disabled={isDisabled}
            />
            <ComboboxTrigger
              aria-label="Show environments"
              disabled={isDisabled}
            />
          </ComboboxInputGroup>
          <ComboboxContent>
            <ComboboxList>
              <ComboboxItem value="production">Production</ComboboxItem>
              <ComboboxItem value="staging">Staging</ComboboxItem>
              <ComboboxItem value="development">Development</ComboboxItem>
            </ComboboxList>
          </ComboboxContent>
        </Combobox>
      )}
    </FieldExample>
  );
}

function ComboboxSection() {
  return (
    <CatalogSection
      id="combobox"
      title="Combobox"
      description="Filterable select; type to narrow the options."
    >
      <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
        <ComboboxField label="Empty" state="empty" />
        <ComboboxField label="Filled" state="filled" />
        <ComboboxField label="Error" state="error" />
        <ComboboxField label="Disabled" state="disabled" />
      </div>
    </CatalogSection>
  );
}

function CheckboxSection() {
  const base = useId();

  return (
    <CatalogSection id="checkbox" title="Checkbox">
      <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
        <Field orientation="horizontal">
          <Checkbox id={`${base}-unchecked`} aria-label="Unchecked" />
          <FieldLabel htmlFor={`${base}-unchecked`}>Unchecked</FieldLabel>
        </Field>
        <Field orientation="horizontal">
          <Checkbox
            id={`${base}-checked`}
            aria-label="Checked"
            defaultChecked
          />
          <FieldLabel htmlFor={`${base}-checked`}>Checked</FieldLabel>
        </Field>
        <Field invalid>
          <FieldLabel htmlFor={`${base}-error`}>Accept the terms</FieldLabel>
          <Checkbox
            id={`${base}-error`}
            aria-label="Accept the terms"
            aria-describedby={`${base}-error-message`}
            aria-invalid
          />
          <FieldError id={`${base}-error-message`} match>
            You must accept the terms to continue.
          </FieldError>
        </Field>
        <Field disabled orientation="horizontal">
          <Checkbox id={`${base}-disabled`} aria-label="Disabled" disabled />
          <FieldLabel htmlFor={`${base}-disabled`}>Disabled</FieldLabel>
        </Field>
      </div>
    </CatalogSection>
  );
}

function SwitchSection() {
  const base = useId();

  return (
    <CatalogSection
      id="switch"
      title="Switch"
      description="For settings that apply immediately; never a two-option select."
    >
      <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
        <Field orientation="horizontal">
          <Switch id={`${base}-off`} aria-label="Off switch" />
          <FieldLabel htmlFor={`${base}-off`}>Off</FieldLabel>
        </Field>
        <Field orientation="horizontal">
          <Switch id={`${base}-on`} aria-label="On switch" defaultChecked />
          <FieldLabel htmlFor={`${base}-on`}>On</FieldLabel>
        </Field>
        <Field invalid>
          <FieldLabel htmlFor={`${base}-error`}>Require approval</FieldLabel>
          <Switch
            id={`${base}-error`}
            aria-label="Require approval"
            aria-describedby={`${base}-error-message`}
            aria-invalid
          />
          <FieldError id={`${base}-error-message`} match>
            Approval settings could not be saved.
          </FieldError>
        </Field>
        <Field disabled orientation="horizontal">
          <Switch
            id={`${base}-disabled`}
            aria-label="Disabled switch"
            disabled
          />
          <FieldLabel htmlFor={`${base}-disabled`}>Disabled</FieldLabel>
        </Field>
      </div>
    </CatalogSection>
  );
}

function RadioGroupDemo({
  label,
  state,
}: {
  label: string;
  state: 'empty' | 'filled' | 'error' | 'disabled';
}) {
  const base = useId();
  const isDisabled = state === 'disabled';

  return (
    <Field disabled={isDisabled} invalid={state === 'error'}>
      <FieldLabel>{label}</FieldLabel>
      <RadioGroup
        aria-label={label}
        aria-describedby={
          state === 'error' ? `${base}-error-message` : undefined
        }
        aria-invalid={state === 'error' || undefined}
        defaultValue={state === 'filled' ? 'weekly' : undefined}
        disabled={isDisabled}
      >
        {['daily', 'weekly', 'never'].map((value) => (
          <div key={value} className="flex items-center gap-2">
            <RadioGroupItem
              id={`${base}-${value}`}
              value={value}
              aria-label={
                value === 'daily'
                  ? 'Daily'
                  : value === 'weekly'
                    ? 'Weekly'
                    : 'Never'
              }
            />
            <FieldLabel>
              {value === 'daily'
                ? 'Daily'
                : value === 'weekly'
                  ? 'Weekly'
                  : 'Never'}
            </FieldLabel>
          </div>
        ))}
      </RadioGroup>
      {state === 'error' ? (
        <FieldError id={`${base}-error-message`} match>
          Choose a digest frequency.
        </FieldError>
      ) : null}
    </Field>
  );
}

function RadioGroupSection() {
  return (
    <CatalogSection id="radio-group" title="RadioGroup">
      <div className="grid gap-6 md:grid-cols-2 xl:grid-cols-4">
        <RadioGroupDemo label="Empty" state="empty" />
        <RadioGroupDemo label="Filled" state="filled" />
        <RadioGroupDemo label="Error" state="error" />
        <RadioGroupDemo label="Disabled" state="disabled" />
      </div>
    </CatalogSection>
  );
}

function TabsSection() {
  const orgTabs = [
    { value: 'general', label: 'General', count: null },
    { value: 'members', label: 'Members', count: '12' },
    { value: 'invites', label: 'Invites', count: '2' },
    { value: 'billing', label: 'Billing', count: null },
    { value: 'security', label: 'Security', count: null },
    { value: 'danger', label: 'Danger', count: null },
  ] as const;

  return (
    <CatalogSection
      id="tabs"
      title="Tabs"
      description="Underline style, 36px tall, bound to ?tab= in the app. Arrow keys move between tabs."
    >
      <State title="3 tabs">
        <Tabs defaultValue="overview" className="w-full">
          <TabsList aria-label="App sections">
            <TabsTrigger value="overview">Overview</TabsTrigger>
            <TabsTrigger value="explorer">Explorer</TabsTrigger>
            <TabsTrigger value="settings">Settings</TabsTrigger>
          </TabsList>
          <TabsContent value="overview">Overview panel content.</TabsContent>
          <TabsContent value="explorer">Explorer panel content.</TabsContent>
          <TabsContent value="settings">Settings panel content.</TabsContent>
        </Tabs>
      </State>
      <State title="6 tabs, with a badge count">
        <Tabs defaultValue="members" className="w-full">
          <TabsList aria-label="Organization sections">
            {orgTabs.map((tab) => (
              <TabsTrigger key={tab.value} value={tab.value}>
                {tab.label}
                {tab.count ? (
                  <Badge tone="neutral" aria-hidden="true">
                    {tab.count}
                  </Badge>
                ) : null}
              </TabsTrigger>
            ))}
          </TabsList>
          {orgTabs.map((tab) => (
            <TabsContent key={tab.value} value={tab.value}>
              {tab.label} panel content.
            </TabsContent>
          ))}
        </Tabs>
      </State>
    </CatalogSection>
  );
}

function BadgeSection() {
  return (
    <CatalogSection id="badge" title="Badge">
      <State title="All tones">
        <Badge tone="neutral">Neutral</Badge>
        <Badge tone="accent">Accent</Badge>
        <Badge tone="success">Success</Badge>
        <Badge tone="warning">Warning</Badge>
        <Badge tone="destructive">Destructive</Badge>
      </State>
    </CatalogSection>
  );
}

function StatusPillSection() {
  return (
    <CatalogSection
      id="status-pill"
      title="StatusPill"
      description="Colour is never the only signal: every pill carries text. Only Running pulses."
    >
      <State title="All statuses">
        <StatusPill status="active" />
        <StatusPill status="read-only" />
        <StatusPill status="disabled" />
        <StatusPill status="running" />
        <StatusPill status="failed" />
      </State>
    </CatalogSection>
  );
}

function TooltipSection() {
  return (
    <CatalogSection
      id="tooltip"
      title="Tooltip"
      description="Opens after 400ms on hover, immediately on keyboard focus, and closes on Esc."
    >
      <State title="Hover or focus">
        <Tooltip>
          <TooltipTrigger
            render={<Button variant="secondary">Hover or focus me</Button>}
          />
          <TooltipContent>Tooltips dismiss with Esc.</TooltipContent>
        </Tooltip>
      </State>
    </CatalogSection>
  );
}

function PopoverSection() {
  return (
    <CatalogSection id="popover" title="Popover">
      <State title="Open">
        <Popover>
          <PopoverTrigger
            render={<Button variant="secondary">Open popover</Button>}
          />
          <PopoverContent>
            <PopoverTitle>Saved filters</PopoverTitle>
            <PopoverDescription>
              Keep this filter for everyone in the workspace.
            </PopoverDescription>
            <PopoverClose
              render={<Button variant="secondary" size="sm" className="mt-3" />}
            >
              Close
            </PopoverClose>
          </PopoverContent>
        </Popover>
      </State>
    </CatalogSection>
  );
}

function DropdownMenuSection() {
  return (
    <CatalogSection
      id="dropdown-menu"
      title="DropdownMenu"
      description="Arrow keys and type-ahead work; destructive items are last, after a divider."
    >
      <State title="Open">
        <DropdownMenu>
          <DropdownMenuTrigger
            render={<Button variant="secondary">Open menu</Button>}
          />
          <DropdownMenuContent>
            <DropdownMenuGroup>
              <DropdownMenuLabel>Actions</DropdownMenuLabel>
              <DropdownMenuItem>Duplicate app</DropdownMenuItem>
              <DropdownMenuItem>Transfer app</DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem className="text-destructive">
                Delete app
              </DropdownMenuItem>
            </DropdownMenuGroup>
          </DropdownMenuContent>
        </DropdownMenu>
      </State>
    </CatalogSection>
  );
}

function ContextMenuSection() {
  return (
    <CatalogSection
      id="context-menu"
      title="ContextMenu"
      description="Right-click the target; keyboard users have the same actions in the row menu."
    >
      <State title="Right-click">
        <ContextMenu>
          <ContextMenuTrigger
            render={<Button variant="secondary">Right-click this row</Button>}
          />
          <ContextMenuContent>
            <ContextMenuGroup>
              <ContextMenuLabel>Entity</ContextMenuLabel>
              <ContextMenuItem>Open</ContextMenuItem>
              <ContextMenuItem>Copy ID</ContextMenuItem>
              <ContextMenuSeparator />
              <ContextMenuItem className="text-destructive">
                Delete entity
              </ContextMenuItem>
            </ContextMenuGroup>
          </ContextMenuContent>
        </ContextMenu>
      </State>
    </CatalogSection>
  );
}

function DialogSection() {
  return (
    <CatalogSection
      id="dialog"
      title="Dialog"
      description="Short tasks only (≤ 3 fields), 440px wide. Focus goes to the first field."
    >
      <State title="Form dialog">
        <Dialog>
          <DialogTrigger
            render={<Button variant="secondary">Open dialog</Button>}
          />
          <DialogContent>
            <DialogHeader>
              <DialogTitle>Rename app</DialogTitle>
              <DialogDescription>
                The new name appears in the switcher and in email notifications.
              </DialogDescription>
            </DialogHeader>
            <div className="mt-4">
              <FieldExample label="App name">
                {({ id, describedBy }) => (
                  <Input
                    id={id}
                    aria-describedby={describedBy}
                    defaultValue="todo-app"
                  />
                )}
              </FieldExample>
            </div>
            <DialogFooter>
              <DialogClose
                render={<Button variant="secondary">Cancel</Button>}
              />
              <Button>Save name</Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      </State>
    </CatalogSection>
  );
}

function SheetDemo({
  size,
  state,
}: {
  size: 'sm' | 'md' | 'lg';
  state: 'empty' | 'dirty' | 'saving' | 'error';
}) {
  const isSaving = state === 'saving';
  const isDirty = state === 'dirty' || state === 'saving' || state === 'error';

  return (
    <Sheet>
      <SheetTrigger
        render={
          <Button variant="secondary">
            {state} ({size})
          </Button>
        }
      />
      <SheetContent size={size}>
        <SheetHeader>
          <SheetTitle>Edit webhook</SheetTitle>
          <SheetDescription>
            State: {state}. The composite layer wires Esc, backdrop and Cancel
            through the unsaved-changes guard.
          </SheetDescription>
        </SheetHeader>
        <SheetBody className="flex flex-col gap-4">
          <FieldExample label="Endpoint URL">
            {({ id, describedBy, invalid }) => (
              <Input
                id={id}
                aria-describedby={describedBy}
                aria-invalid={invalid || undefined}
                defaultValue={isDirty ? 'https://example.com/hooks' : undefined}
                placeholder="https://example.com/hooks"
              />
            )}
          </FieldExample>
          <FieldExample
            label="Description"
            description="Optional; shown in the webhook list."
          >
            {({ id, describedBy }) => (
              <Textarea
                id={id}
                aria-describedby={describedBy}
                defaultValue={isDirty ? 'Deploy notifications' : undefined}
              />
            )}
          </FieldExample>
          {state === 'error' ? (
            <p className="text-destructive text-sm" role="alert">
              Couldn&apos;t save the webhook. Request ID: req_01HXYZ.
            </p>
          ) : null}
        </SheetBody>
        <SheetFooter>
          <SheetClose render={<Button variant="secondary">Cancel</Button>} />
          <Button disabled={isSaving || !isDirty}>
            {isSaving ? 'Saving…' : 'Save webhook'}
          </Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}

function SheetSection() {
  return (
    <CatalogSection
      id="sheet"
      title="Sheet"
      description="Right side, widths sm/md/lg. Header, scrollable body and sticky footer."
    >
      <State title="Empty, dirty, saving, error">
        <SheetDemo size="sm" state="empty" />
        <SheetDemo size="md" state="dirty" />
        <SheetDemo size="lg" state="saving" />
        <SheetDemo size="md" state="error" />
      </State>
    </CatalogSection>
  );
}

function ConfirmDialogSection() {
  const [open, setOpen] = useState(false);

  return (
    <CatalogSection
      id="confirm-dialog"
      title="ConfirmDialog"
      description="Tier 2: names the object and the consequence; the button repeats the verb."
    >
      <State title="Destructive confirmation">
        <Button
          variant="destructive"
          onClick={() => {
            setOpen(true);
          }}
        >
          Delete webhook
        </Button>
        <ConfirmDialog
          open={open}
          onOpenChange={setOpen}
          title="Delete the deploy webhook?"
          description="BackendRelay stops sending deploy events immediately. This cannot be undone."
          confirmLabel="Delete webhook"
          confirming={false}
          onConfirm={() => {
            setOpen(false);
          }}
        />
      </State>
    </CatalogSection>
  );
}

function TypedConfirmDialogSection() {
  const [lastOpen, setLastOpen] = useState(false);
  const [restoreOpen, setRestoreOpen] = useState(false);

  return (
    <CatalogSection
      id="typed-confirm-dialog"
      title="TypedConfirmDialog"
      description="Tier 3: the destructive button stays disabled until the typed text matches exactly (case-sensitive). Type the shown text to see the matching state."
    >
      <State title="Not matching (type the app title to unlock)">
        <Button
          variant="destructive"
          onClick={() => {
            setLastOpen(true);
          }}
        >
          Delete app
        </Button>
        <TypedConfirmDialog
          open={lastOpen}
          onOpenChange={setLastOpen}
          title="Delete todo-app?"
          description="The app, its data and its files are removed permanently."
          consequences={[
            'Every client using this app stops working.',
            'All entities, users and stored files are deleted.',
            'Backups are kept according to the workspace retention policy.',
          ]}
          confirmText="todo-app"
          confirmLabel="Delete app"
          confirming={false}
          onConfirm={() => {
            setLastOpen(false);
          }}
        />
      </State>
      <State title="Matching (type restore to unlock)">
        <Button
          variant="secondary"
          onClick={() => {
            setRestoreOpen(true);
          }}
        >
          Restore backup
        </Button>
        <TypedConfirmDialog
          open={restoreOpen}
          onOpenChange={setRestoreOpen}
          title="Restore this backup?"
          description="A new app is created from the snapshot; the current app is untouched."
          consequences={[
            'The restore job starts immediately.',
            'The new app uses storage until you delete it.',
          ]}
          confirmText="restore"
          confirmLabel="Restore backup"
          confirming={false}
          onConfirm={() => {
            setRestoreOpen(false);
          }}
        />
      </State>
    </CatalogSection>
  );
}

function ToastSection() {
  return (
    <CatalogSection
      id="toast"
      title="Toast"
      description="Bottom-right, 360px. Success closes after 4s; errors persist and use high priority."
    >
      <ToastTriggers />
    </CatalogSection>
  );
}

function ToastTriggers() {
  const toast = useToast();

  return (
    <State title="Success, error with details, action">
      <Button
        variant="secondary"
        onClick={() => {
          toast.add({
            type: 'success',
            title: 'App created',
            description: 'todo-app is ready to use.',
          });
        }}
      >
        Success toast
      </Button>
      <Button
        variant="secondary"
        onClick={() => {
          toast.add({
            type: 'error',
            title: "Couldn't save changes",
            description: 'Request ID: req_01HXYZ. The request timed out.',
          });
        }}
      >
        Error toast
      </Button>
      <Button
        variant="secondary"
        onClick={() => {
          toast.add({
            title: 'Webhook paused',
            description: 'Deliveries stop until you resume it.',
            actionProps: { children: 'Undo' },
          });
        }}
      >
        Toast with action
      </Button>
    </State>
  );
}

function PageHeaderSection() {
  return (
    <CatalogSection
      id="page-header"
      title="PageHeader"
      description="h1, optional description, right-aligned actions (one primary) and optional tabs."
    >
      <PageHeader
        title="Explorer"
        description="Browse and edit entities in the todo-app namespace."
        actions={
          <>
            <Button variant="secondary">
              <RefreshCw aria-hidden="true" />
              Refresh
            </Button>
            <Button>
              <Plus aria-hidden="true" />
              New entity
            </Button>
            <IconButton
              aria-label="More actions"
              icon={<MoreHorizontal />}
              size="sm"
            />
          </>
        }
        tabs={
          <Tabs defaultValue="data">
            <TabsList aria-label="Explorer views">
              <TabsTrigger value="data">Data</TabsTrigger>
              <TabsTrigger value="relationships">Relationships</TabsTrigger>
            </TabsList>
          </Tabs>
        }
      />
    </CatalogSection>
  );
}

function SectionCardSection() {
  return (
    <CatalogSection id="section-card" title="SectionCard">
      <SectionCard
        title="Webhooks"
        description="Delivery and retry settings for this app."
        actions={
          <Button variant="secondary" size="sm">
            Edit
          </Button>
        }
        footer={
          <Button size="sm" onClick={noop}>
            Save changes
          </Button>
        }
      >
        <p className="text-muted-foreground text-sm">
          Deliveries are retried with exponential backoff for up to 24 hours.
        </p>
      </SectionCard>
    </CatalogSection>
  );
}

function SettingsRowSection() {
  const enabledId = useId();
  const retriesId = useId();

  return (
    <CatalogSection id="settings-row" title="SettingsRow">
      <SectionCard title="Webhook delivery">
        <div className="flex flex-col gap-4">
          <SettingsRow
            htmlFor={enabledId}
            label="Enabled"
            description="Pause deliveries without deleting the endpoint."
          >
            <Switch
              id={enabledId}
              aria-label="Webhook delivery enabled"
              defaultChecked
            />
          </SettingsRow>
          <SettingsRow
            htmlFor={retriesId}
            label="Retries"
            description="Maximum attempts before the delivery is marked failed."
          >
            <Input id={retriesId} defaultValue="5" className="w-20" />
          </SettingsRow>
        </div>
      </SectionCard>
    </CatalogSection>
  );
}

function UnsavedChangesBarSection() {
  return (
    <CatalogSection
      id="unsaved-changes-bar"
      title="UnsavedChangesBar"
      description="Floating at bottom centre in the app; inline here so all three states fit."
    >
      <div className="flex flex-col gap-3">
        <State title="Dirty">
          <UnsavedChangesBar
            dirty
            onReset={noop}
            onSave={noop}
            className="static bottom-auto left-auto translate-x-0"
          />
        </State>
        <State title="Saving">
          <UnsavedChangesBar
            dirty
            saving
            onReset={noop}
            onSave={noop}
            className="static bottom-auto left-auto translate-x-0"
          />
        </State>
        <State title="Error">
          <UnsavedChangesBar
            dirty
            error="Couldn't save changes."
            onReset={noop}
            onSave={noop}
            className="static bottom-auto left-auto translate-x-0"
          />
        </State>
      </div>
    </CatalogSection>
  );
}

function EmptyStateSection() {
  return (
    <CatalogSection id="empty-state" title="EmptyState">
      <State title="With primary action and secondary link">
        <EmptyState
          icon={Users}
          title="No users yet"
          description="Users appear here after they sign in or you add them."
          action={<Button>Invite a user</Button>}
          secondary={
            <Button variant="link" size="sm">
              Read the auth guide
            </Button>
          }
        />
      </State>
      <State title="Minimal">
        <EmptyState
          icon={Boxes}
          title="No apps yet"
          description="Create your first app to get started."
        />
      </State>
    </CatalogSection>
  );
}

function ErrorStateSection() {
  return (
    <CatalogSection
      id="error-state"
      title="ErrorState"
      description="500, 403 and app-404 variants, with request ID, Retry and details."
    >
      <div className="grid gap-4 xl:grid-cols-2">
        <State title="500">
          <ErrorState
            thing="users"
            status={500}
            message="The server couldn't complete the request."
            type="InternalServerError"
            requestId="req_01HXYZ"
            hint={{ retryable: true }}
            onRetry={noop}
          />
        </State>
        <State title="403">
          <ErrorState
            thing="schema"
            status={403}
            message="Only admins can change the schema."
            requiredRole="Admin"
            requestId="req_01HABC"
          />
        </State>
        <State title="404 (app)">
          <ErrorState thing="app" status={404} kind="app" />
        </State>
      </div>
    </CatalogSection>
  );
}

function SkeletonsSection() {
  return (
    <CatalogSection
      id="skeletons"
      title="Skeleton patterns"
      description="Each matches the final layout. Delays are disabled here so the placeholders are visible."
    >
      <State title="TableSkeleton">
        <div className="w-full">
          <TableSkeleton rows={3} columns={4} delayMs={0} />
        </div>
      </State>
      <State title="CardSkeleton">
        <div className="w-full max-w-md">
          <CardSkeleton delayMs={0} />
        </div>
      </State>
      <State title="FormSkeleton">
        <div className="w-full max-w-md">
          <FormSkeleton rows={3} delayMs={0} />
        </div>
      </State>
    </CatalogSection>
  );
}

function IdChipSection() {
  return (
    <CatalogSection id="id-chip" title="IdChip">
      <State title="Truncated, copy on click, full value in tooltip">
        <IdChip value="3f2a1b2c-3d4e-5f6a-7b8c-9c1d2e3f4a5b" />
      </State>
    </CatalogSection>
  );
}

function CopyButtonSection() {
  return (
    <CatalogSection id="copy-button" title="CopyButton">
      <State title="Icon only and with a label">
        <CopyButton value="https://example.com/hooks" />
        <CopyButton
          value="https://example.com/hooks"
          label="endpoint"
          size="md"
        />
      </State>
    </CatalogSection>
  );
}

function RevealedSecretDemo() {
  const containerRef = useRef<HTMLDivElement>(null);
  const revealedOnce = useRef(false);

  useEffect(() => {
    const container = containerRef.current;
    if (!container || revealedOnce.current) {
      return;
    }
    revealedOnce.current = true;
    // Render the real revealed state once, through the same Show action the
    // user has. The ref keeps StrictMode from toggling it back.
    container
      .querySelector<HTMLButtonElement>('button[aria-label="Show secret"]')
      ?.click();
  }, []);

  return (
    <div ref={containerRef}>
      <SecretField value="revealed_9f8e7d6c5b4a3210" prefixLength={10} />
    </div>
  );
}

function SecretFieldSection() {
  return (
    <CatalogSection
      id="secret-field"
      title="SecretField"
      description="Click Show secret to see the revealed state; it auto-hides after 30s."
    >
      <State title="Hidden, revealed, with Rotate">
        <SecretField value="a1b2c3d4e5f6a7b8c9d0" />
        <RevealedSecretDemo />
        <SecretField
          value="whsec_9f8e7d6c5b4a3210"
          prefixLength={10}
          onRotate={noop}
        />
      </State>
    </CatalogSection>
  );
}

function KeyValueListSection() {
  return (
    <CatalogSection id="key-value-list" title="KeyValueList">
      <State title="Metadata">
        <KeyValueList
          items={[
            { label: 'App ID', value: <IdChip value="app_01hxyz" /> },
            { label: 'Region', value: 'eu-central' },
            {
              label: 'Created',
              value: <RelativeTime value="2026-10-26T09:00:00Z" />,
            },
            { label: 'Storage', value: <Bytes value={1_234_567} /> },
          ]}
        />
      </State>
    </CatalogSection>
  );
}

function RelativeTimeSection() {
  const now = new Date('2026-10-26T12:00:00Z');

  return (
    <CatalogSection id="relative-time" title="RelativeTime">
      <State title="Relative label, exact time in the tooltip">
        <RelativeTime value="2026-10-26T11:57:00Z" now={now} />
        <RelativeTime value="2026-10-25T12:00:00Z" now={now} />
        <RelativeTime value="not-a-date" now={now} />
      </State>
    </CatalogSection>
  );
}

function FormattersSection() {
  return (
    <CatalogSection id="formatters" title="Bytes and Count">
      <State title="Formatted values">
        <Bytes value={1_234_567} />
        <Bytes value={42} />
        <Count value={12_345} />
        <Count value={7} />
      </State>
    </CatalogSection>
  );
}

function SearchInputSection() {
  const [empty, setEmpty] = useState('');
  const [filled, setFilled] = useState('ada');

  return (
    <CatalogSection
      id="search-input"
      title="SearchInput"
      description="Esc clears the text first, then blurs; it never closes the enclosing sheet."
    >
      <State title="Empty with / hint">
        <div className="w-72">
          <SearchInput
            aria-label="Search users"
            value={empty}
            onValueChange={setEmpty}
            placeholder="Search users"
            showShortcut
          />
        </div>
      </State>
      <State title="Filled with clear button">
        <div className="w-72">
          <SearchInput
            aria-label="Search users filled"
            value={filled}
            onValueChange={setFilled}
            placeholder="Search users"
          />
        </div>
      </State>
    </CatalogSection>
  );
}

function filterCondition(
  attribute: string,
  operator: FilterCondition['operator'],
  value: string,
): FilterCondition {
  return { ...createFilterCondition(attribute, operator), value };
}

function FilterDemo({ count }: { count: 0 | 2 }) {
  const [conditions, setConditions] = useState<FilterCondition[]>(
    count === 2
      ? [
          filterCondition('status', '$eq', 'active'),
          filterCondition('visits', '$gt', '10'),
        ]
      : [],
  );

  return (
    <div className="flex flex-col gap-3">
      <FilterButton
        attributes={FILTER_ATTRIBUTES}
        conditions={conditions}
        onChange={setConditions}
      />
      <div className="border-border rounded-md border p-3">
        <FilterPanel
          attributes={FILTER_ATTRIBUTES}
          conditions={conditions}
          onChange={setConditions}
        />
      </div>
    </div>
  );
}

function FilterSection() {
  return (
    <CatalogSection
      id="filter"
      title="FilterButton and FilterPanel"
      description="The button shows the active count and opens the panel; rows edit attribute, operator and value."
    >
      <State title="0 filters">
        <div className="w-full max-w-3xl">
          <FilterDemo count={0} />
        </div>
      </State>
      <State title="2 filters">
        <div className="w-full max-w-3xl">
          <FilterDemo count={2} />
        </div>
      </State>
    </CatalogSection>
  );
}

function PaginationSection() {
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(50);

  return (
    <CatalogSection id="pagination" title="Pagination">
      <State title="Known total">
        <div className="w-full">
          <Pagination
            page={page}
            pageSize={pageSize}
            total={312}
            onPageChange={setPage}
            onPageSizeChange={(size) => {
              setPageSize(size);
              setPage(1);
            }}
          />
        </div>
      </State>
      <State title="Unknown total (50+)">
        <div className="w-full">
          <Pagination
            page={2}
            pageSize={50}
            total={null}
            onPageChange={noop}
            hasNext
          />
        </div>
      </State>
      <State title="Hidden when everything fits on one page">
        <Pagination page={1} pageSize={50} total={12} onPageChange={noop} />
        <p className="text-muted-foreground text-xs">
          Nothing renders for a known total that fits in one page.
        </p>
      </State>
    </CatalogSection>
  );
}

function SelectionTableDemo() {
  const [selected, setSelected] = useState<string[]>(['1', '3']);

  return (
    <SimpleTable
      caption="Selected users"
      columns={TABLE_COLUMNS}
      rows={TABLE_ROWS}
      getRowId={(row) => row.id}
      selectable
      selectedIds={selected}
      onSelectionChange={setSelected}
      bulkActions={() => (
        <Button variant="destructive" size="sm">
          <Trash2 aria-hidden="true" />
          Delete selected
        </Button>
      )}
    />
  );
}

function SimpleTableSection() {
  return (
    <CatalogSection
      id="simple-table"
      title="SimpleTable"
      description="Sticky header, sortable headers with aria-sort, optional selection and a bulk bar."
    >
      <State title="Loading">
        <div className="w-full">
          <SimpleTable
            caption="Loading users"
            columns={TABLE_COLUMNS}
            rows={[]}
            getRowId={(row) => row.id}
            loading
          />
        </div>
      </State>
      <State title="Empty">
        <div className="w-full">
          <SimpleTable
            caption="Empty users"
            columns={TABLE_COLUMNS}
            rows={[]}
            getRowId={(row) => row.id}
            empty="No users match these filters."
          />
        </div>
      </State>
      <State title="Rows">
        <div className="w-full">
          <SimpleTable
            caption="Users"
            columns={TABLE_COLUMNS}
            rows={TABLE_ROWS}
            getRowId={(row) => row.id}
            sort={{ id: 'name', dir: 'asc' }}
            onSortChange={noop}
          />
        </div>
      </State>
      <State title="Selection">
        <div className="w-full">
          <SelectionTableDemo />
        </div>
      </State>
    </CatalogSection>
  );
}

function DataGridSection() {
  return (
    <CatalogSection
      id="data-grid"
      title="DataGrid"
      description="AG Grid Community, themed from CSS variables, a single Tab stop and ⌘C to copy a cell."
    >
      <div className="h-72 w-full" data-testid="catalog-data-grid">
        <DataGrid<GridRow>
          aria-label="Example entities"
          rowData={GRID_ROWS}
          columnDefs={GRID_COLUMNS}
          getRowId={(row) => row.id}
        />
      </div>
    </CatalogSection>
  );
}

function CodeEditorSection() {
  const [value, setValue] = useState(JSON_SAMPLE);

  return (
    <CatalogSection
      id="code-editor"
      title="CodeEditor"
      description="Tab is never captured; Esc then Tab leaves the editor. Lint problems appear in the footer."
    >
      <State title="Valid JSON">
        <div className="w-full">
          <CodeEditor
            aria-label="Valid JSON example"
            mode="json"
            value={value}
            onChange={setValue}
            height="150px"
          />
        </div>
      </State>
      <State title="JSON with errors">
        <div className="w-full">
          <CodeEditor
            aria-label="Invalid JSON example"
            mode="json"
            value={INVALID_JSON}
            height="150px"
          />
        </div>
      </State>
      <State title="Read-only TypeScript">
        <div className="w-full">
          <CodeEditor
            aria-label="Read-only TypeScript example"
            mode="readonly-ts"
            value={TYPESCRIPT_SAMPLE}
            height="150px"
            readOnly
          />
        </div>
      </State>
    </CatalogSection>
  );
}

function CodeBlockSection() {
  return (
    <CatalogSection id="code-block" title="CodeBlock">
      <State title="JSON snippet with copy">
        <div className="w-full">
          <CodeBlock language="json" code={JSON_SAMPLE} />
        </div>
      </State>
    </CatalogSection>
  );
}

function KbdSection() {
  return (
    <CatalogSection id="kbd" title="Kbd">
      <State title="Single key and chord">
        <Kbd>⌘K</Kbd>
        <KbdGroup>
          <Kbd>G</Kbd>
          <Kbd>E</Kbd>
        </KbdGroup>
        <Kbd>?</Kbd>
      </State>
    </CatalogSection>
  );
}

function ChartWithRange({ variant }: { variant: 'line' | 'bar' }) {
  const [range, setRange] = useState('7d');

  return (
    <Chart
      title={variant === 'line' ? 'Live sessions' : 'Transactions per day'}
      description="Last 7 days"
      data={CHART_DATA}
      xKey="day"
      series={[
        {
          key: 'sessions',
          label: variant === 'line' ? 'Sessions' : 'Transactions',
        },
      ]}
      variant={variant}
      ranges={[
        { value: '24h', label: 'Last 24 hours' },
        { value: '7d', label: 'Last 7 days' },
        { value: '30d', label: 'Last 30 days' },
      ]}
      range={range}
      onRangeChange={setRange}
      height={180}
    />
  );
}

function ChartSection() {
  return (
    <CatalogSection
      id="chart"
      title="Chart"
      description="Recharts via the shadcn wrapper. Every chart has Show as table."
    >
      <div className="grid gap-4 xl:grid-cols-2">
        <ChartWithRange variant="line" />
        <ChartWithRange variant="bar" />
      </div>
      <State title="Empty">
        <div className="w-full">
          <Chart
            title="Backup size"
            data={[]}
            xKey="day"
            series={[{ key: 'bytes', label: 'Bytes' }]}
            emptyMessage="Backups will appear here after the first snapshot."
          />
        </div>
      </State>
    </CatalogSection>
  );
}

function CommandPaletteSection() {
  const [open, setOpen] = useState(false);

  return (
    <CatalogSection
      id="command-palette"
      title="CommandPalette"
      description="The palette composite belongs to B.7; this is the Command shell and dialog it will use."
    >
      <State title="Command dialog">
        <Button
          variant="secondary"
          onClick={() => {
            setOpen(true);
          }}
        >
          Open command palette
        </Button>
        <CommandDialog open={open} onOpenChange={setOpen}>
          <CommandInput
            aria-label="Search apps and actions"
            placeholder="Search apps and actions…"
          />
          <CommandList>
            <CommandEmpty>No results found.</CommandEmpty>
            <CommandGroup heading="Apps">
              <CommandItem
                onSelect={() => {
                  setOpen(false);
                }}
              >
                todo-app
                <CommandShortcut>⌘1</CommandShortcut>
              </CommandItem>
              <CommandItem
                onSelect={() => {
                  setOpen(false);
                }}
              >
                notes-app
                <CommandShortcut>⌘2</CommandShortcut>
              </CommandItem>
            </CommandGroup>
            <CommandGroup heading="Actions">
              <CommandItem
                onSelect={() => {
                  setOpen(false);
                }}
              >
                Create app
                <CommandShortcut>⌘N</CommandShortcut>
              </CommandItem>
            </CommandGroup>
          </CommandList>
        </CommandDialog>
      </State>
      <State title="Inline command list">
        <div className="border-border w-full max-w-md rounded-lg border">
          <Command aria-label="Inline command list">
            <CommandInput
              aria-label="Inline command search"
              placeholder="Type a command…"
            />
            <CommandList>
              <CommandEmpty>No commands.</CommandEmpty>
              <CommandGroup heading="Navigation">
                <CommandItem>Go to Explorer</CommandItem>
                <CommandItem>Go to Schema</CommandItem>
              </CommandGroup>
            </CommandList>
          </Command>
        </div>
      </State>
    </CatalogSection>
  );
}
