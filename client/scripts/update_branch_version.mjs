#!/usr/bin/env node

import { readFileSync, writeFileSync } from "node:fs";

const { BRANCH_NAME: branchName, GITHUB_RUN_ID: runId, GITHUB_RUN_ATTEMPT: runAttempt } = process.env;
if (!branchName || !runId || !runAttempt) {
  throw new Error("BRANCH_NAME, GITHUB_RUN_ID, and GITHUB_RUN_ATTEMPT are required");
}
const file = "version.ts";
const source = readFileSync(file, "utf8");
const match = source.match(/const version = '(v\d+\.\d+\.\d+)'/);
if (!match) throw new Error(`Could not find base version in ${file}`);
const newVersion = `${match[1]}-branch-${branchName}.${runId}.${runAttempt}`;
writeFileSync(file, source.replace(match[1], newVersion));
console.log(`Updated ${file} to ${newVersion}`);
