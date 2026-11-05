import {
  APPLE_DISCOVERY_ENDPOINT,
  GOOGLE_DISCOVERY_ENDPOINT,
  LINKEDIN_DISCOVERY_ENDPOINT,
} from '@instantdb/platform';
import { useId, useState, type ReactNode } from 'react';
import { CopyButton } from '@/components/data/copy-button';
import { Button } from '@/components/ui/button';
import {
  Field,
  FieldContent,
  FieldDescription,
  FieldLabel,
} from '@/components/ui/field';
import { Input } from '@/components/ui/input';
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
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { Switch } from '@/components/ui/switch';
import { Textarea } from '@/components/ui/textarea';
import { isApiError } from '@/lib/api/client';
import type {
  OAuthClient,
  OAuthClientBody,
  OAuthClientUpdateBody,
  OAuthProvider,
} from '@/lib/api/endpoints';
import {
  GOOGLE_APP_TYPES,
  GOOGLE_APP_TYPE_LABELS,
  appleClientMeta,
  clerkDiscoveryEndpoint,
  clerkDomain,
  findClientName,
  firebaseDiscoveryEndpoint,
  firebaseProjectIdFrom,
  googleAppTypeFrom,
  metaBoolean,
  metaString,
  parseAudienceList,
  redirectUri,
  usedClientNames,
  type AuthProviderDefinition,
  type GoogleAppType,
} from '@/lib/app-auth/providers';
import {
  useCreateClient,
  useCreateProvider,
  useUpdateClient,
} from '@/lib/app-auth/queries';
import { getRuntimeConfig } from '@/lib/runtime-config';
import { toastManager } from '@/lib/toast';

export interface ProviderSheetProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  appId: string;
  provider: AuthProviderDefinition;
  /** The provider record when one already exists for this app. */
  existingProvider: OAuthProvider | undefined;
  /** The client being edited; `undefined` opens the add form. */
  client: OAuthClient | undefined;
  clients: ReadonlyArray<OAuthClient>;
  /** Switches the same sheet to the add form. */
  onAddAnother: () => void;
  /** Providers and clients need the collaborator role (C.11). */
  canManage: boolean;
}

interface ClientDraft {
  clientName: string;
  clientId: string;
  clientSecret: string;
  extraAudiences: string;
  appType: GoogleAppType;
  useSharedCredentials: boolean;
  servicesId: string;
  teamId: string;
  keyId: string;
  privateKey: string;
  publishableKey: string;
  allowUnverifiedEmail: boolean;
  projectId: string;
}

function initialDraft(
  provider: AuthProviderDefinition,
  client: OAuthClient | undefined,
  clients: ReadonlyArray<OAuthClient>,
): ClientDraft {
  if (client === undefined) {
    const used = usedClientNames(clients);
    return {
      clientName: findClientName(provider.clientPrefix, used),
      clientId: '',
      clientSecret: '',
      extraAudiences: '',
      appType: 'web',
      useSharedCredentials: false,
      servicesId: '',
      teamId: '',
      keyId: '',
      privateKey: '',
      publishableKey: '',
      allowUnverifiedEmail: false,
      projectId: '',
    };
  }
  return {
    clientName: client.client_name,
    clientId: client.client_id ?? '',
    clientSecret: '',
    extraAudiences: (client.extra_audiences ?? []).join(', '),
    appType: googleAppTypeFrom(client.meta),
    useSharedCredentials: client.use_shared_credentials === true,
    servicesId: client.client_id ?? '',
    teamId: metaString(client.meta, 'teamId'),
    keyId: metaString(client.meta, 'keyId'),
    privateKey: '',
    publishableKey: metaString(client.meta, 'clerkPublishableKey'),
    allowUnverifiedEmail: metaBoolean(client.meta, 'allowUnverifiedEmail'),
    projectId: firebaseProjectIdFrom(client.discovery_endpoint ?? undefined),
  };
}

function formProblem(
  definition: AuthProviderDefinition,
  draft: ClientDraft,
  editing: boolean,
  clients: ReadonlyArray<OAuthClient>,
): string | undefined {
  if (!editing) {
    const name = draft.clientName.trim();
    if (name === '') return 'Enter a client name.';
    if (clients.some((client) => client.client_name === name)) {
      return `The client name "${name}" is already in use.`;
    }
  }
  switch (definition.id) {
    case 'google':
      if (!draft.useSharedCredentials) {
        if (draft.clientId.trim() === '') return 'Enter the client ID.';
        if (!editing && draft.appType === 'web' && draft.clientSecret === '') {
          return 'Enter the client secret.';
        }
      }
      if (parseAudienceList(draft.extraAudiences).length > 20) {
        return 'Use at most 20 audience IDs.';
      }
      if (
        parseAudienceList(draft.extraAudiences).some(
          (audience) => audience.length > 256,
        )
      ) {
        return 'Audience IDs must be 256 characters or fewer.';
      }
      return undefined;
    case 'apple': {
      if (draft.servicesId.trim() === '') return 'Enter the Services ID.';
      const keyParts = [draft.teamId, draft.keyId, draft.privateKey].filter(
        (part) => part.trim() !== '',
      );
      if (keyParts.length > 0 && keyParts.length < 3) {
        return 'Enter the Team ID, Key ID and Private Key together, or leave all three blank.';
      }
      return undefined;
    }
    case 'github':
    case 'linkedin':
      if (draft.clientId.trim() === '') return 'Enter the client ID.';
      if (!editing && draft.clientSecret === '') {
        return 'Enter the client secret.';
      }
      return undefined;
    case 'clerk':
      if (!draft.publishableKey.startsWith('pk_')) {
        return 'The Clerk publishable key should start with "pk_".';
      }
      if (clerkDomain(draft.publishableKey) === undefined) {
        return 'Could not determine the Clerk domain from that key.';
      }
      return undefined;
    case 'firebase':
      if (draft.projectId.trim() === '')
        return 'Enter the Firebase project ID.';
      return undefined;
  }
}

function createBody(
  definition: AuthProviderDefinition,
  draft: ClientDraft,
  providerId: string,
): OAuthClientBody {
  const base: OAuthClientBody = {
    provider_id: providerId,
    client_name: draft.clientName.trim(),
  };
  switch (definition.id) {
    case 'google':
      if (draft.useSharedCredentials) {
        return {
          ...base,
          use_shared_credentials: true,
          discovery_endpoint: GOOGLE_DISCOVERY_ENDPOINT,
          extra_audiences: parseAudienceList(draft.extraAudiences),
          meta: { skipNonceChecks: true, appType: draft.appType },
        };
      }
      return {
        ...base,
        client_id: draft.clientId.trim(),
        client_secret: draft.clientSecret,
        discovery_endpoint: GOOGLE_DISCOVERY_ENDPOINT,
        extra_audiences: parseAudienceList(draft.extraAudiences),
        meta: { skipNonceChecks: true, appType: draft.appType },
      };
    case 'apple':
      return {
        ...base,
        client_id: draft.servicesId.trim(),
        ...(draft.privateKey === '' ? {} : { client_secret: draft.privateKey }),
        discovery_endpoint: APPLE_DISCOVERY_ENDPOINT,
        meta: appleClientMeta(draft.teamId, draft.keyId) ?? undefined,
      };
    case 'github':
      return {
        ...base,
        client_id: draft.clientId.trim(),
        client_secret: draft.clientSecret,
      };
    case 'linkedin':
      return {
        ...base,
        client_id: draft.clientId.trim(),
        client_secret: draft.clientSecret,
        discovery_endpoint: LINKEDIN_DISCOVERY_ENDPOINT,
      };
    case 'clerk':
      return {
        ...base,
        discovery_endpoint: clerkDiscoveryEndpoint(draft.publishableKey),
        meta: {
          clerkPublishableKey: draft.publishableKey,
          allowUnverifiedEmail: draft.allowUnverifiedEmail,
        },
      };
    case 'firebase':
      return {
        ...base,
        discovery_endpoint: firebaseDiscoveryEndpoint(draft.projectId),
      };
  }
}

function updateBody(
  definition: AuthProviderDefinition,
  draft: ClientDraft,
): OAuthClientUpdateBody {
  switch (definition.id) {
    case 'google':
      return {
        ...(draft.useSharedCredentials
          ? { use_shared_credentials: true }
          : {
              client_id: draft.clientId.trim(),
              ...(draft.clientSecret === ''
                ? {}
                : { client_secret: draft.clientSecret }),
            }),
        extra_audiences: parseAudienceList(draft.extraAudiences),
        meta: { skipNonceChecks: true, appType: draft.appType },
      };
    case 'apple':
      return {
        client_id: draft.servicesId.trim(),
        ...(draft.privateKey === '' ? {} : { client_secret: draft.privateKey }),
        meta: appleClientMeta(draft.teamId, draft.keyId) ?? undefined,
      };
    case 'github':
    case 'linkedin':
      return {
        client_id: draft.clientId.trim(),
        ...(draft.clientSecret === ''
          ? {}
          : { client_secret: draft.clientSecret }),
      };
    case 'clerk':
      return {
        discovery_endpoint: clerkDiscoveryEndpoint(draft.publishableKey),
        meta: {
          clerkPublishableKey: draft.publishableKey,
          allowUnverifiedEmail: draft.allowUnverifiedEmail,
        },
      };
    case 'firebase':
      return {
        discovery_endpoint: firebaseDiscoveryEndpoint(draft.projectId),
      };
  }
}

interface TextFieldProps {
  label: ReactNode;
  value: string;
  onChange: (value: string) => void;
  type?: 'text' | 'password';
  placeholder?: string;
  description?: ReactNode;
  autoComplete?: string;
}

function TextField({
  label,
  value,
  onChange,
  type = 'text',
  placeholder,
  description,
  autoComplete = 'off',
}: TextFieldProps) {
  const id = useId();
  return (
    <Field>
      <FieldLabel htmlFor={id}>{label}</FieldLabel>
      <Input
        id={id}
        type={type}
        value={value}
        placeholder={placeholder}
        autoComplete={autoComplete}
        onChange={(event) => {
          onChange(event.target.value);
        }}
      />
      {description ? <FieldDescription>{description}</FieldDescription> : null}
    </Field>
  );
}

interface TextAreaFieldProps {
  label: ReactNode;
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  description?: ReactNode;
}

function TextAreaField({
  label,
  value,
  onChange,
  placeholder,
  description,
}: TextAreaFieldProps) {
  const id = useId();
  return (
    <Field>
      <FieldLabel htmlFor={id}>{label}</FieldLabel>
      <Textarea
        id={id}
        rows={6}
        value={value}
        placeholder={placeholder}
        onChange={(event) => {
          onChange(event.target.value);
        }}
      />
      {description ? <FieldDescription>{description}</FieldDescription> : null}
    </Field>
  );
}

interface SwitchFieldProps {
  label: string;
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
  description?: ReactNode;
}

function SwitchField({
  label,
  checked,
  onCheckedChange,
  description,
}: SwitchFieldProps) {
  return (
    <Field orientation="horizontal">
      <FieldContent>
        <FieldLabel>{label}</FieldLabel>
        {description ? (
          <FieldDescription>{description}</FieldDescription>
        ) : null}
      </FieldContent>
      <Switch
        aria-label={label}
        checked={checked}
        onCheckedChange={onCheckedChange}
      />
    </Field>
  );
}

function RedirectUriField() {
  const value = redirectUri(getRuntimeConfig()?.apiBaseUrl);
  return (
    <Field>
      <FieldLabel>Redirect URI</FieldLabel>
      <div className="border-input bg-muted flex min-h-8 items-center gap-1 rounded-md border px-2 py-1">
        <code className="min-w-0 flex-1 truncate font-mono text-xs">
          {value}
        </code>
        <CopyButton value={value} aria-label="Copy redirect URI" />
      </div>
      <FieldDescription>
        Add this URI to the provider&apos;s OAuth app settings.
      </FieldDescription>
    </Field>
  );
}

/**
 * C.11 provider sheet (672px): the add form for `?add=` and the edit form for
 * `?client=`. Secrets are write-only, so an edit leaves the field blank to keep
 * the stored value.
 */
export function ProviderSheet({
  open,
  onOpenChange,
  appId,
  provider,
  existingProvider,
  client,
  clients,
  onAddAnother,
  canManage,
}: ProviderSheetProps) {
  const editing = client !== undefined;
  const createProvider = useCreateProvider(appId);
  const createClient = useCreateClient(appId);
  const updateClient = useUpdateClient(appId);
  const [draft, setDraft] = useState<ClientDraft>(() =>
    initialDraft(provider, client, clients),
  );
  const [error, setError] = useState<string>();
  const [saving, setSaving] = useState(false);

  function patch(next: Partial<ClientDraft>): void {
    setDraft((current) => ({ ...current, ...next }));
    setError(undefined);
  }

  async function save(): Promise<void> {
    if (saving) return;
    const problem = formProblem(provider, draft, editing, clients);
    if (problem !== undefined) {
      setError(problem);
      return;
    }
    setError(undefined);
    setSaving(true);
    try {
      if (client !== undefined) {
        await updateClient.mutateAsync({
          clientRecordId: client.id,
          body: updateBody(provider, draft),
        });
        toastManager.add({
          type: 'success',
          title: `${provider.label} client updated`,
        });
        onOpenChange(false);
        return;
      }

      let providerRecord = existingProvider;
      if (providerRecord === undefined) {
        const created = await createProvider.mutateAsync(provider.id);
        providerRecord = created.provider;
      }
      await createClient.mutateAsync({
        body: createBody(provider, draft, providerRecord.id),
      });
      toastManager.add({
        type: 'success',
        title: `Added ${provider.label} client`,
      });
      onOpenChange(false);
    } catch (failure) {
      setError(
        isApiError(failure)
          ? failure.message
          : `Couldn't save the ${provider.label} client.`,
      );
    } finally {
      setSaving(false);
    }
  }

  const busy =
    saving ||
    createProvider.isPending ||
    createClient.isPending ||
    updateClient.isPending;

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent size="md">
        <SheetHeader>
          <SheetTitle>
            {client === undefined
              ? `Add ${provider.label} client`
              : `${provider.label} client`}
          </SheetTitle>
          <SheetDescription>
            {editing
              ? 'Update the credentials this app uses to sign users in.'
              : `Register a ${provider.label} app so users can sign in with it.`}
          </SheetDescription>
        </SheetHeader>
        <SheetBody>
          <div className="flex flex-col gap-4">
            {error ? (
              <p role="alert" className="text-destructive text-sm font-medium">
                {error}
              </p>
            ) : null}

            {client !== undefined ? (
              <Field>
                <FieldLabel>Client name</FieldLabel>
                <p className="text-cell font-mono">{client.client_name}</p>
              </Field>
            ) : (
              <TextField
                label="Client name"
                value={draft.clientName}
                placeholder={`e.g. ${provider.clientPrefix}`}
                onChange={(value) => {
                  patch({ clientName: value });
                }}
              />
            )}

            {provider.id === 'google' ? (
              <>
                <Field>
                  <FieldLabel>Client type</FieldLabel>
                  <Select
                    value={draft.appType}
                    onValueChange={(value) => {
                      if (typeof value === 'string') {
                        const appType = value;
                        patch({
                          appType,
                          ...(appType === 'web'
                            ? {}
                            : { useSharedCredentials: false }),
                        });
                      }
                    }}
                  >
                    <SelectTrigger aria-label="Client type">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      {GOOGLE_APP_TYPES.map((appType) => (
                        <SelectItem key={appType} value={appType}>
                          {GOOGLE_APP_TYPE_LABELS[appType]}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                  <FieldDescription>
                    {draft.appType === 'web'
                      ? 'Redirect-based web sign-in.'
                      : 'Native sign-in with your own Google client ID.'}
                  </FieldDescription>
                </Field>
                {draft.appType === 'web' ? (
                  <SwitchField
                    label="Use shared credentials"
                    checked={draft.useSharedCredentials}
                    description="Development only. Turn this off and add your own credentials before production."
                    onCheckedChange={(checked) => {
                      patch({ useSharedCredentials: checked });
                    }}
                  />
                ) : null}
                {draft.useSharedCredentials ? null : (
                  <>
                    <TextField
                      label="Client ID"
                      value={draft.clientId}
                      onChange={(value) => {
                        patch({ clientId: value });
                      }}
                    />
                    {draft.appType === 'web' ? (
                      <TextField
                        label="Client secret"
                        type="password"
                        autoComplete="new-password"
                        value={draft.clientSecret}
                        description={
                          editing
                            ? 'Leave blank to keep the current secret.'
                            : undefined
                        }
                        onChange={(value) => {
                          patch({ clientSecret: value });
                        }}
                      />
                    ) : null}
                  </>
                )}
                <TextField
                  label="Extra audience IDs"
                  value={draft.extraAudiences}
                  placeholder="com.example.ios, com.example.android"
                  description="Optional comma-separated IDs accepted in the token's audience claim."
                  onChange={(value) => {
                    patch({ extraAudiences: value });
                  }}
                />
              </>
            ) : null}

            {provider.id === 'apple' ? (
              <>
                <TextField
                  label="Services ID"
                  value={draft.servicesId}
                  onChange={(value) => {
                    patch({ servicesId: value });
                  }}
                />
                <TextField
                  label="Team ID"
                  value={draft.teamId}
                  onChange={(value) => {
                    patch({ teamId: value });
                  }}
                />
                <TextField
                  label="Key ID"
                  value={draft.keyId}
                  onChange={(value) => {
                    patch({ keyId: value });
                  }}
                />
                <TextAreaField
                  label="Private key"
                  value={draft.privateKey}
                  placeholder={
                    '-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----'
                  }
                  description={
                    editing
                      ? 'Leave blank to keep the current key.'
                      : 'Required only for the web redirect flow.'
                  }
                  onChange={(value) => {
                    patch({ privateKey: value });
                  }}
                />
              </>
            ) : null}

            {provider.id === 'github' || provider.id === 'linkedin' ? (
              <>
                <TextField
                  label="Client ID"
                  value={draft.clientId}
                  onChange={(value) => {
                    patch({ clientId: value });
                  }}
                />
                <TextField
                  label="Client secret"
                  type="password"
                  autoComplete="new-password"
                  value={draft.clientSecret}
                  description={
                    editing
                      ? 'Leave blank to keep the current secret.'
                      : undefined
                  }
                  onChange={(value) => {
                    patch({ clientSecret: value });
                  }}
                />
              </>
            ) : null}

            {provider.id === 'clerk' ? (
              <>
                <TextField
                  label="Publishable key"
                  value={draft.publishableKey}
                  placeholder="pk_test_…"
                  onChange={(value) => {
                    patch({ publishableKey: value });
                  }}
                />
                <SwitchField
                  label="Allow unverified email"
                  checked={draft.allowUnverifiedEmail}
                  description="Store an email even when the Clerk token doesn't mark it verified."
                  onCheckedChange={(checked) => {
                    patch({ allowUnverifiedEmail: checked });
                  }}
                />
                <p className="text-muted-foreground text-xs">
                  Clerk session tokens must include an <code>aud</code> claim
                  that matches this client. Add it in the Clerk dashboard under
                  Sessions → Customize session token.
                </p>
              </>
            ) : null}

            {provider.id === 'firebase' ? (
              <TextField
                label="Project ID"
                value={draft.projectId}
                placeholder="my-firebase-project"
                onChange={(value) => {
                  patch({ projectId: value });
                }}
              />
            ) : null}

            {provider.id === 'clerk' && draft.publishableKey !== '' ? (
              <Field>
                <FieldLabel>Clerk domain</FieldLabel>
                <p className="text-cell font-mono">
                  {clerkDomain(draft.publishableKey) ?? 'Unknown'}
                </p>
              </Field>
            ) : null}

            <RedirectUriField />
          </div>
        </SheetBody>
        <SheetFooter>
          {editing ? (
            <Button
              type="button"
              variant="ghost"
              className="mr-auto"
              disabled={!canManage}
              onClick={onAddAnother}
            >
              Add another client
            </Button>
          ) : null}
          <Button
            type="button"
            variant="secondary"
            onClick={() => {
              onOpenChange(false);
            }}
          >
            Cancel
          </Button>
          <Button
            type="button"
            disabled={busy || !canManage}
            onClick={() => {
              void save();
            }}
          >
            {busy ? 'Saving…' : editing ? 'Save' : 'Add client'}
          </Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}
