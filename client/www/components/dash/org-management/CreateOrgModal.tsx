import { useCallback, useContext, useState } from 'react';
import { Dialog, useDialog } from '@/components/ui';
import { useFetchedDash } from '../MainDashLayout';
import { CreationWorkspace } from '../creation/CreationWorkspace';
import { creationHref, type Request } from '../creation/model';
import { jsonFetch } from '@/lib/fetch';
import config from '@/lib/config';
import { TokenContext } from '@/lib/contexts';
import { useReadyRouter } from '@/components/clientOnlyPage';

export const CreateOrgModal = ({
  dialog,
}: {
  dialog: ReturnType<typeof useDialog>;
}) => {
  const dash = useFetchedDash(),
    token = useContext(TokenContext),
    router = useReadyRouter();
  const [busy, setBusy] = useState(false);
  const request: Request = useCallback(
    (path, init) =>
      jsonFetch(`${config.apiURI}${path}`, {
        ...init,
        headers: {
          ...init?.headers,
          authorization: `Bearer ${token}`,
          'content-type': 'application/json',
        },
      }),
    [token],
  );
  return (
    <Dialog
      title="Create organization"
      open={dialog.open}
      onClose={() => {
        if (!busy) dialog.onClose();
      }}
      hideCloseButton={busy}
    >
      {dialog.open && (
        <CreationWorkspace
          kind="organization"
          compact
          accountId={dash.data.user.id}
          availableTargets={[]}
          request={request}
          onBusyChange={setBusy}
          onCancel={() => dialog.onClose()}
          onCreated={() => {}}
          onOpen={async (result) => {
            await dash.refetch();
            dash.setWorkspace(result.id);
            router.push(creationHref(result));
            dialog.onClose();
          }}
        />
      )}
    </Dialog>
  );
};
