#!/usr/bin/env node

// Publishes the kept SDK packages to npm under the BackendRelay scope (D2).
//
// The workspace keeps upstream's @instantdb/* package names so the lockfile,
// the turbo filters and the acceptance suites that import package names stay
// untouched. Publishing therefore works on a staged copy of each package:
//
//   * the manifest name becomes @backendrelay/<directory>,
//   * workspace: dependency ranges become the version being published,
//   * sibling imports in the build output move to the new scope (third-party
//     @instantdb packages such as @instantdb/eventsource keep their names),
//   * homepage and repository point at this repository, not upstream's.
//
// Nothing in the working tree is modified, so a failed run leaves no
// half-versioned package.json files behind. docs/sdk-publishing.md documents
// the owner's first publish, the Verdaccio rehearsal and the CI tag flow.

import { execFileSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptPath = fileURLToPath(import.meta.url);
const clientRoot = dirname(dirname(scriptPath));

export const SCOPE = "@backendrelay";
export const REPOSITORY = "https://github.com/orenaksakal/backendrelay";
export const VERSION_FILE = "packages/version/src/version.ts";

// The kept packages, dependencies before dependents, so a run that fails part
// way leaves the registry consistent. Everything else under client/packages is
// private to the workspace and is never published.
export const PACKAGES = [
  "version",
  "core",
  "react-common",
  "webhooks",
  "admin",
  "platform",
  "react",
  "react-native",
  "react-native-mmkv",
  "expo-sqlite",
  "components",
  "resumable-stream",
  "cli",
];

const WORKSPACE_SCOPE = "@instantdb/";
const DEPENDENCY_FIELDS = [
  "dependencies",
  "devDependencies",
  "peerDependencies",
  "optionalDependencies",
];
const TEXT_EXTENSIONS = new Set([
  ".js",
  ".mjs",
  ".cjs",
  ".jsx",
  ".ts",
  ".tsx",
  ".map",
  ".md",
  ".css",
  ".json",
]);
// A published package has no use for a lockfile or a leftover tarball.
const SKIPPED_NAMES = new Set([
  "node_modules",
  ".turbo",
  "pnpm-lock.yaml",
  "package-lock.json",
  "yarn.lock",
]);

export function publishedName(directory) {
  return `${SCOPE}/${directory}`;
}

export function readVersion(root = clientRoot) {
  const source = readFileSync(join(root, VERSION_FILE), "utf8");
  const match = source.match(/const version = '(v[^']+)'/);
  if (!match) {
    throw new Error(`Could not find the SDK version in ${VERSION_FILE}`);
  }
  return match[1].slice(1);
}

// Rewrites one manifest for publication. `source` is the package's own
// package.json; `directory` is its entry in PACKAGES.
export function rewriteManifest(source, directory, version) {
  const manifest = JSON.parse(source);
  manifest.name = publishedName(directory);
  manifest.version = version;
  manifest.homepage = `${REPOSITORY}/tree/main/client/packages/${directory}`;
  manifest.repository = {
    type: "git",
    url: `git+${REPOSITORY}.git`,
    directory: `client/packages/${directory}`,
  };
  for (const field of DEPENDENCY_FIELDS) {
    const dependencies = manifest[field];
    if (!dependencies) continue;
    const rewritten = {};
    for (const [name, range] of Object.entries(dependencies)) {
      const sibling = name.startsWith(WORKSPACE_SCOPE)
        ? name.slice(WORKSPACE_SCOPE.length)
        : undefined;
      if (sibling === undefined || !PACKAGES.includes(sibling)) {
        rewritten[name] = range;
        continue;
      }
      // workspace:* / workspace:^ only describe the local link; consumers need
      // the packages published in this run.
      rewritten[publishedName(sibling)] = range.startsWith("workspace:")
        ? `^${version}`
        : range;
    }
    manifest[field] = rewritten;
  }
  return manifest;
}

// Rewrites sibling package names in build output and documentation. Only the
// kept packages move: @instantdb/eventsource and other upstream packages stay
// as they are, and the trailing lookahead keeps @instantdb/react from matching
// @instantdb/react-common.
export function rewriteScope(source) {
  let rewritten = source;
  for (const directory of PACKAGES) {
    const name = `${WORKSPACE_SCOPE}${directory}`;
    rewritten = rewritten.replace(
      new RegExp(`${name}(?![\\w.-])`, "g"),
      publishedName(directory),
    );
  }
  return rewritten;
}

function stagePackage(directory, stagingDir, version) {
  mkdirSync(stagingDir, { recursive: true });
  cpSync(join(clientRoot, "packages", directory), stagingDir, {
    recursive: true,
    filter: (source) => {
      const name = source.slice(source.lastIndexOf("/") + 1);
      return !SKIPPED_NAMES.has(name);
    },
  });
  const manifestPath = join(stagingDir, "package.json");
  writeFileSync(
    manifestPath,
    `${JSON.stringify(
      rewriteManifest(readFileSync(manifestPath, "utf8"), directory, version),
      null,
      2,
    )}\n`,
  );
  rewriteTree(stagingDir);
}

function rewriteTree(directory) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      rewriteTree(path);
      continue;
    }
    if (!TEXT_EXTENSIONS.has(extname(entry.name))) continue;
    const source = readFileSync(path, "utf8");
    const rewritten = rewriteScope(source);
    if (rewritten !== source) writeFileSync(path, rewritten);
  }
}

function usage() {
  return [
    "Usage: node scripts/publish_packages.mjs [dist-tag] [options]",
    "",
    "  dist-tag            publish under this npm dist-tag (for example",
    "                      experimental); the version must contain it",
    "  --package <dir>     publish only these packages (repeatable)",
    "  --dry-run           report what npm would upload, without publishing",
    "  --registry <url>    publish to another registry (Verdaccio rehearsal)",
    "  --help              show this text",
  ].join("\n");
}

function parseArgs(argv) {
  const options = {
    tag: undefined,
    packages: [],
    dryRun: false,
    registry: undefined,
    help: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--help" || argument === "-h") {
      options.help = true;
    } else if (argument === "--dry-run") {
      options.dryRun = true;
    } else if (argument === "--package") {
      const directory = argv[index + 1];
      if (!directory) throw new Error("--package needs a directory name");
      options.packages.push(directory);
      index += 1;
    } else if (argument === "--registry") {
      const registry = argv[index + 1];
      if (!registry) throw new Error("--registry needs a URL");
      options.registry = registry;
      index += 1;
    } else if (argument.startsWith("-")) {
      throw new Error(`Unknown option ${argument}\n\n${usage()}`);
    } else if (options.tag === undefined) {
      options.tag = argument;
    } else {
      throw new Error(`Unexpected argument ${argument}\n\n${usage()}`);
    }
  }
  return options;
}

function validate(options, version) {
  if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
    throw new Error(
      `${VERSION_FILE} must hold a semver version, found ${version}`,
    );
  }
  if (options.tag) {
    if (!version.includes(options.tag)) {
      throw new Error(
        `When publishing the ${options.tag} tag the version must contain it ` +
          `(for example 1.1.0-${options.tag}.0); ${VERSION_FILE} has ${version}`,
      );
    }
  } else if (!/^\d+\.\d+\.\d+$/.test(version)) {
    throw new Error(
      `${VERSION_FILE} has the prerelease ${version}; pass a dist-tag ` +
        `(for example experimental) instead of publishing it as latest`,
    );
  }
  for (const directory of options.packages) {
    if (!PACKAGES.includes(directory)) {
      throw new Error(
        `${directory} is not a published package; choose one of ` +
          PACKAGES.join(", "),
      );
    }
  }
}

function publishCommand(stagingDir, options) {
  const args = [
    "publish",
    stagingDir,
    "--access",
    "public",
    "--tag",
    options.tag ?? "latest",
    // Everything is built already; a workspace lifecycle script would run in
    // the staged copy and only add a failure mode.
    "--ignore-scripts",
  ];
  if (options.dryRun) {
    // npm still packs the directory and prints every file it would upload.
    args.push("--dry-run");
  } else if (
    process.env.PUBLISH_PROVENANCE
      ? process.env.PUBLISH_PROVENANCE !== "0"
      : process.env.GITHUB_ACTIONS === "true"
  ) {
    // Provenance needs the workflow's OIDC token, so it only runs in CI unless
    // PUBLISH_PROVENANCE overrides it.
    args.push("--provenance");
  }
  if (options.registry) args.push("--registry", options.registry);
  return args;
}

function main(argv) {
  const options = parseArgs(argv);
  if (options.help) {
    console.log(usage());
    return;
  }
  const version = readVersion();
  validate(options, version);
  const selected = options.packages.length > 0 ? options.packages : PACKAGES;
  const missing = selected.filter(
    (directory) => !existsSync(join(clientRoot, "packages", directory, "dist")),
  );
  if (missing.length > 0) {
    throw new Error(
      `packages/${missing.join(", packages/")} have no dist/; run ` +
        `\`pnpm build-packages\` first (make publish builds it for you)`,
    );
  }
  console.log(
    `Publishing ${selected.length} package(s) as ${SCOPE}/* at ${version} ` +
      `(dist-tag ${options.tag ?? "latest"}${options.dryRun ? ", dry run" : ""})`,
  );

  const stagingRoot = mkdtempSync(join(tmpdir(), "backendrelay-publish-"));
  const published = [];
  try {
    for (const directory of selected) {
      const stagingDir = join(stagingRoot, directory);
      stagePackage(directory, stagingDir, version);
      const args = publishCommand(stagingDir, options);
      const command = args.join(" ").replace(stagingDir, "<staging>");
      console.log(`\n[${publishedName(directory)}] npm ${command}`);
      execFileSync("npm", args, { cwd: clientRoot, stdio: "inherit" });
      published.push(directory);
    }
  } catch (error) {
    if (published.length > 0) {
      const remaining = selected.filter(
        (directory) => !published.includes(directory),
      );
      console.error(
        `\nAlready published: ${published.map(publishedName).join(", ")}`,
      );
      if (remaining.length > 0) {
        console.error(
          `Resume with: node scripts/publish_packages.mjs ${remaining
            .map((directory) => `--package ${directory}`)
            .join(" ")}`,
        );
      }
    }
    throw error;
  } finally {
    rmSync(stagingRoot, { recursive: true, force: true });
  }

  console.log(
    `\nPublished ${published.length} package(s) at ${version}` +
      `${options.registry ? ` to ${options.registry}` : ""}`,
  );
}

if (process.argv[1] && resolve(process.argv[1]) === scriptPath) {
  try {
    main(process.argv.slice(2));
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exitCode = 1;
  }
}
