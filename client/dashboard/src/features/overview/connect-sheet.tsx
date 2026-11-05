import { useState } from 'react';
import { CodeBlock } from '@/components/code-editor/code-block';
import { CopyButton } from '@/components/data/copy-button';
import { KeyValueList } from '@/components/data/key-value-list';
import { Button } from '@/components/ui/button';
import {
  Sheet,
  SheetBody,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import type { App } from '@/lib/api/endpoints/schemas';
import {
  connectSnippets,
  type ConnectTabId,
  type ConnectDetails,
} from '@/lib/sdk';

const CONNECT_TABS: ReadonlyArray<{ id: ConnectTabId; label: string }> = [
  { id: 'react', label: 'React' },
  { id: 'react-native', label: 'React Native' },
  { id: 'admin', label: 'Admin SDK' },
  { id: 'cli', label: 'CLI' },
];

function isConnectTabId(value: unknown): value is ConnectTabId {
  return CONNECT_TABS.some((tab) => tab.id === value);
}

function CopyValue({ label, value }: { label: string; value: string }) {
  return (
    <span className="flex items-start gap-2">
      <code className="text-cell min-w-0 font-mono break-all">{value}</code>
      <CopyButton value={value} aria-label={`Copy ${label}`} />
    </span>
  );
}

export interface ConnectSheetProps {
  app: App;
  details: ConnectDetails;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/** C.5 Connect sheet: IDs, endpoints and ready-to-paste SDK snippets. */
export function ConnectSheet({
  app,
  details,
  open,
  onOpenChange,
}: ConnectSheetProps) {
  const [tab, setTab] = useState<ConnectTabId>('react');
  const snippets = connectSnippets(details);

  const items = [
    {
      label: 'App ID',
      value: <CopyValue label="app ID" value={app.id} />,
    },
    {
      label: 'API URL',
      value:
        details.apiUrl === '' ? (
          <span className="text-muted-foreground">Unavailable</span>
        ) : (
          <CopyValue label="API URL" value={details.apiUrl} />
        ),
    },
    {
      label: 'WebSocket URL',
      value:
        details.websocketUrl === '' ? (
          <span className="text-muted-foreground">Unavailable</span>
        ) : (
          <CopyValue label="WebSocket URL" value={details.websocketUrl} />
        ),
    },
  ];

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent size="md">
        <SheetHeader>
          <SheetTitle>Connect to {app.title}</SheetTitle>
          <SheetDescription>
            Use these values in a client SDK or with the BackendRelay CLI.
          </SheetDescription>
        </SheetHeader>
        <SheetBody className="flex flex-col gap-4">
          <KeyValueList items={items} />

          <Tabs
            value={tab}
            onValueChange={(value) => {
              if (isConnectTabId(value)) setTab(value);
            }}
          >
            <TabsList aria-label="Connect snippets">
              {CONNECT_TABS.map((entry) => (
                <TabsTrigger key={entry.id} value={entry.id}>
                  {entry.label}
                </TabsTrigger>
              ))}
            </TabsList>
            {CONNECT_TABS.map((entry) => {
              const snippet = snippets[entry.id];
              return (
                <TabsContent
                  key={entry.id}
                  value={entry.id}
                  className="flex flex-col gap-3"
                >
                  {snippet.install === undefined ? null : (
                    <CodeBlock code={snippet.install} language="bash" />
                  )}
                  <CodeBlock code={snippet.code} language={snippet.language} />
                </TabsContent>
              );
            })}
          </Tabs>
        </SheetBody>
        <SheetFooter>
          <Button
            type="button"
            variant="secondary"
            onClick={() => {
              onOpenChange(false);
            }}
          >
            Done
          </Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}
