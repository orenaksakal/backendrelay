import path from 'node:path';
import {
  getEmailReadCandidates,
  getPermsReadCandidates,
  getSchemaReadCandidates,
} from './findConfigCandidates.ts';
import { loadConfig } from './loadConfig.ts';

const readLocalConfigFile = async (candidates: any) => {
  const res = await loadConfig<any>({
    sources: candidates,
    merge: false,
  });
  if (!res.config) return;
  return {
    path: path.relative(process.cwd(), res.sources[0]!),
    config: res.config,
  };
};

export const readLocalPermsFile = async () => {
  const result = await readLocalConfigFile(getPermsReadCandidates());
  return result && { path: result.path, perms: result.config };
};

export const readLocalSchemaFile = async () => {
  const result = await readLocalConfigFile(getSchemaReadCandidates());
  return result && { path: result.path, schema: result.config };
};

export const readLocalEmailFile = async (emailPath?: string) => {
  const result = await readLocalConfigFile(getEmailReadCandidates(emailPath));
  return result && { path: result.path, email: result.config };
};
