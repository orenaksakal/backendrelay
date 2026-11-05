import useSWR from 'swr';
import { useReadScope } from './useReadScope';
import { jsonFetch } from '../fetch';
import { readRetryPolicy } from '../request';
import { APIResponse, optimisticUpdate, useAuthToken } from '../auth';
import config from '../config';
import { InstantApp } from '../types';
import { useDashFetch } from './useDashFetch';
import { Role } from '@/pages/dash';
import { workspaceCacheKey } from '@/components/dash/shell/model';

export type OrgInvite = {
  id: string;
  email: string;
  role: string;
  status: string;
  sent_at: string;
  expired: boolean;
};

type ApiOrgResponse = {
  id: string;
  apps: InstantApp[];
  org: {
    id: string;
    title: string;
    created_at: string;
    updated_at: string;
    role: string;
    paid: boolean;
  };
  members: {
    id: string;
    email: string;
    role: Role;
  }[];
  invites: OrgInvite[];
};

export type OrgWorkspace = {
  type: 'org';
} & ApiOrgResponse;

export type Workspace = { type: 'personal'; apps: InstantApp[] } | OrgWorkspace;

const getOrgDetails = async (params: {
  orgId: string;
  token: string;
  signal?: AbortSignal;
}): Promise<Workspace> => {
  const jsonRes = await jsonFetch(
    `${config.apiURI}/dash/orgs/${params.orgId}`,
    {
      signal: params.signal,
      headers: { authorization: `Bearer ${params.token}` },
    },
  );

  if (
    !jsonRes.org ||
    jsonRes.org.id !== params.orgId ||
    !Array.isArray(jsonRes.apps) ||
    !Array.isArray(jsonRes.members) ||
    !Array.isArray(jsonRes.invites)
  )
    throw new Error('The organization response is incomplete.');
  return {
    id: jsonRes.org.id,
    apps: jsonRes.apps,
    org: jsonRes.org,
    members: jsonRes.members,
    invites: jsonRes.invites,
    type: 'org',
  };
};

export const useWorkspace = (
  dashResult: ReturnType<typeof useDashFetch>,
  workspaceId: string,
): APIResponse<Workspace> => {
  const token = useAuthToken();
  const read = useReadScope(JSON.stringify(['workspace', workspaceId, token]));

  const result = useSWR(
    workspaceCacheKey(token, workspaceId),
    () =>
      read(async (signal) => {
        if (!token) {
          throw new Error('Unauthorized');
        }
        return getOrgDetails({
          signal,
          orgId: workspaceId,
          token: token,
        });
      }),
    readRetryPolicy,
  );

  const optimistic: APIResponse<Workspace>['optimisticUpdate'] = (
    mutationPromiseToWaitFor,
    optimisticDataProducer,
  ) => {
    return optimisticUpdate(
      result,
      mutationPromiseToWaitFor,
      optimisticDataProducer,
    ) as any;
  };

  if (!token) {
    return {
      optimisticUpdate: optimistic,
      ...result,
      data: undefined,
      error: new Error('Unauthorized'),
      isLoading: false,
    };
  }

  if (workspaceId === 'personal')
    if (dashResult.data) {
      return {
        ...result,
        optimisticUpdate: optimistic,
        data: { apps: dashResult.data.apps, type: 'personal' },
        error: null,
        isLoading: false,
      };
    } else {
      {
        return {
          ...result,
          optimisticUpdate: optimistic,
          data: undefined,
          error: null,
          isLoading: true,
        };
      }
    }
  return {
    ...result,
    optimisticUpdate: optimistic,
  };
};
