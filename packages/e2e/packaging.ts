// The packages as a user installs them: restate-sessions, its addon's package
// for this machine, and restate-agents packed (pnpm pack, which builds their
// JavaScript), checked (publint, and Are the Types Wrong?), then installed
// with npm into a new project with pi's harness alone. A database opens
// through the addon, the agent runs and type-checks, the other harnesses'
// subpaths fail for their missing packages, and the install is weighed.
//
//   pnpm --filter restate-sessions build:dist   # the addon as a package ships it
//   pnpm --filter e2e packaging
//
// PACKAGES=<dir> takes the tarballs there instead, every platform's addon
// among them, as the CI packs them (.github/workflows/packages.yml).
import { execFileSync, type ExecFileSyncOptions } from 'node:child_process';
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { platform } from '../sessions/src/platform.ts';

const root = new URL('../../', import.meta.url).pathname;
const bin = (name: string) => join(root, 'node_modules/.bin', name);
const run = (file: string, args: string[], options: ExecFileSyncOptions = {}) =>
  execFileSync(file, args, { stdio: 'inherit', ...options });
const output = (file: string, args: string[], cwd: string) =>
  execFileSync(file, args, { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });

// Kept if a check fails, to look into.
const at = mkdtempSync(join(tmpdir(), 'restate-packaging-'));
console.log(`in ${at}`);

// Packed, as published: the JavaScript in dist/, and the exports to it.
const packages = process.env.PACKAGES ?? at;
if (!process.env.PACKAGES) {
  const addon = `./packages/sessions/npm/${platform()}`;
  const filters = ['restate-sessions', 'restate-agents', addon].flatMap((name) => ['--filter', name]);
  run('pnpm', [...filters, 'pack', '--pack-destination', at], { cwd: root, stdio: ['ignore', 'ignore', 'inherit'] });
}
const tarballs = readdirSync(packages).filter((name) => name.endsWith('.tgz'));
const tarball = (name: string) =>
  join(
    packages,
    tarballs.find((t) => t.slice(0, t.lastIndexOf('-')) === name)!,
  );
// Each platform's addon, which npm installs on its platform alone.
const addons = Object.fromEntries(
  tarballs
    .filter((t) => t.startsWith('restate-sessions-') && !/^restate-sessions-\d/.test(t))
    .map((t) => [t.slice(0, t.lastIndexOf('-')), `file:${join(packages, t)}`]),
);
for (const name of ['restate-sessions', 'restate-agents']) {
  run(bin('publint'), ['run', tarball(name), '--strict']);
  // ESM only: CommonJS imports them with import().
  run(bin('attw'), [tarball(name), '--profile', 'esm-only', '--no-emoji', '--format', 'table-flipped']);
}

// A user's project, with pi: what restate-agents/pi needs beside restate-agents.
const agents = JSON.parse(readFileSync(join(root, 'packages/agents/package.json'), 'utf8')) as {
  devDependencies: Record<string, string>;
};
const pi = ['@earendil-works/pi-durable', '@earendil-works/pi-ai', '@earendil-works/chord'];
const app = join(at, 'app');
mkdirSync(app);
writeFileSync(
  join(app, 'package.json'),
  JSON.stringify({
    name: 'app',
    private: true,
    type: 'module',
    dependencies: {
      'restate-sessions': `file:${tarball('restate-sessions')}`,
      'restate-agents': `file:${tarball('restate-agents')}`,
      ...Object.fromEntries(pi.map((name) => [name, agents.devDependencies[name]])),
    },
    // Not published: from their tarballs, each skipped on another platform.
    optionalDependencies: addons,
  }),
);
run('npm', ['install', '--no-audit', '--no-fund', '--loglevel=error'], { cwd: app });

// A database opens through the addon, and the agent runs, from the package's
// JavaScript.
writeFileSync(
  join(app, 'agent.ts'),
  `import { sessionObject } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { open } from 'restate-sessions/internal';

const values = new Map<string, Uint8Array>();
const db = await open('packaging', {
  get: async (key) => values.get(key),
  set: (key, value) => void values.set(key, value),
  clear: (key) => void values.delete(key),
  sync: async () => {},
});
await db.execute('CREATE TABLE t (n INTEGER)');
await db.execute('INSERT INTO t VALUES (42)');
console.log(\`the addon runs SQL: \${JSON.stringify(await db.values('SELECT n FROM t'))}\`);
await db.save();

export const agent = sessionObject({ name: 'agent', handlers: { chat: pi.chat({ system: 'Be brief.' }) } });
console.log(Object.keys(agent).length > 0 ? 'restate-agents/pi runs' : 'no agent');

// Types, not run: a model is a \`provider/model-id\` string.
// @ts-expect-error
export const wrong = () => pi.chat({ model: 42 });
`,
);
run(process.execPath, ['agent.ts'], { cwd: app });

// It type-checks against the package's declarations, which type pi's options
// (skipping libraries' own checks, as pi-ai's fail on a package it lacks).
writeFileSync(
  join(app, 'tsconfig.json'),
  JSON.stringify({
    compilerOptions: {
      target: 'esnext',
      module: 'nodenext',
      strict: true,
      noEmit: true,
      allowImportingTsExtensions: true,
      types: [],
      skipLibCheck: true,
    },
    include: ['agent.ts'],
  }),
);
run(bin('tsc'), ['-p', app]);
console.log('restate-agents/pi type-checks');

// The other harnesses are not installed: their subpaths say what is missing.
// (pi-ai's own @anthropic-ai/sdk, hoisted by npm, is the Claude agent's.)
for (const subpath of ['langgraph', 'opencode', 'deepseek']) {
  try {
    output(process.execPath, ['--input-type=module', '-e', `await import('restate-agents/${subpath}')`], app);
  } catch (error) {
    const missing = /Cannot find (?:package|module) '([^']+)'/.exec(String((error as { stderr?: string }).stderr));
    if (!missing) throw error;
    console.log(`restate-agents/${subpath} needs ${missing[1]}`);
    continue;
  }
  throw new Error(`restate-agents/${subpath} loaded without its harness`);
}

// What the install weighs.
const size = (path: string): number =>
  readdirSync(path, { withFileTypes: true }).reduce((total, entry) => {
    const full = join(path, entry.name);
    if (entry.isSymbolicLink()) return total;
    return total + (entry.isDirectory() ? size(full) : statSync(full).size);
  }, 0);
const installed = output('npm', ['ls', '--all', '--parseable'], app).trim().split('\n').length - 1;
const mb = (bytes: number) => `${(bytes / 1e6).toFixed(1)} MB`;
console.log(`installed: ${installed} packages, ${mb(size(join(app, 'node_modules')))}`);
for (const name of ['restate-sessions', `restate-sessions-${platform()}`, 'restate-agents']) {
  console.log(`  ${name}: ${mb(size(join(app, 'node_modules', name)))} (packed ${mb(statSync(tarball(name)).size)})`);
}
rmSync(at, { recursive: true });
