/** Compare built static import closures; browser transfer/timing gates remain separate. */
import { readFile, readdir, writeFile } from 'node:fs/promises';
import { basename, dirname, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { gzipSync } from 'node:zlib';
import ts from 'typescript';

async function inspect(output) {
  const assets = resolve(output, 'public/assets');
  const names = (await readdir(assets)).filter((name) => name.endsWith('.js'));
  const sources = new Map(await Promise.all(names.map(async (name) => [name, await readFile(resolve(assets, name), 'utf8')])));
  const entries = names.filter((name) => name.startsWith('index-') && sources.get(name).includes('dashboard-page-content'));
  if (entries.length !== 1) throw new Error('Cannot uniquely identify the built dashboard entry');
  const ssr = resolve(output, 'server/_ssr');
  const manifests = (await readdir(ssr)).filter((name) => name.startsWith('_tanstack-start-manifest_'));
  if (manifests.length !== 1) throw new Error('Expected one TanStack build manifest');
  const { tsrStartManifest } = await import(pathToFileURL(resolve(ssr, manifests[0])).href);
  const routes = tsrStartManifest().routes;
  entries.push(...[...routes.__root__.preloads, ...routes['/dash'].preloads].map((path) => basename(path)));
  const visited = new Set();
  const walk = (name) => {
    if (visited.has(name)) return;
    if (!sources.has(name)) throw new Error(`Missing built module: ${name}`);
    visited.add(name);
    const source = ts.createSourceFile(name, sources.get(name), ts.ScriptTarget.Latest, false, ts.ScriptKind.JS);
    for (const statement of source.statements) {
      if ((ts.isImportDeclaration(statement) || ts.isExportDeclaration(statement)) && statement.moduleSpecifier && ts.isStringLiteral(statement.moduleSpecifier)) {
        const specifier = statement.moduleSpecifier.text;
        if (specifier.startsWith('.') && specifier.endsWith('.js')) walk(basename(specifier));
      }
    }
  };
  entries.forEach(walk);
  const chunks = [...visited].sort().map((name) => ({ name, bytes: Buffer.byteLength(sources.get(name)), gzipBytes: gzipSync(sources.get(name)).length }));
  const heavy = names.filter((name) => /^(DataEditor|Execution|Sandbox|AppAuth|Perms|Schema|DeploymentWorkspace|OperationsWorkspace|AutomationWorkspace|RunWorkspace)-/.test(name));
  return {
    staticJsBytes: chunks.reduce((sum, chunk) => sum + chunk.bytes, 0),
    staticGzipBytes: chunks.reduce((sum, chunk) => sum + chunk.gzipBytes, 0), chunks,
    inactiveChunkFiles: heavy, inactiveChunksExcluded: heavy.length > 0 && heavy.every((name) => !visited.has(name)),
  };
}
const [baseline, candidate = '.output', destination] = process.argv.slice(2);
if (!baseline) throw new Error('usage: dashboard-bundle-report.mjs BASELINE_OUTPUT [CANDIDATE_OUTPUT] [REPORT_JSON]');
const report = { schemaVersion: 1, kind: 'dashboard-static-bundle-comparison', generatedAt: new Date().toISOString(),
  limitation: 'Static import closure only. Does not establish actual transferred bytes, real-backend acceptance or route timing.',
  baseline: await inspect(resolve(baseline)), candidate: await inspect(resolve(candidate)) };
const json = JSON.stringify(report, null, 2) + '\n';
if (destination) await writeFile(destination, json);
console.log(JSON.stringify({ baselineJsBytes: report.baseline.staticJsBytes, candidateJsBytes: report.candidate.staticJsBytes,
  baselineGzipBytes: report.baseline.staticGzipBytes, candidateGzipBytes: report.candidate.staticGzipBytes,
  inactiveChunksExcluded: report.candidate.inactiveChunksExcluded, report: destination ?? null }, null, 2));
