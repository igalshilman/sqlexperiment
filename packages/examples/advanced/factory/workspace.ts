/**
 * The coders' workspace, in git: a clone of the project per coder
 * (WORKSPACE/ENG-1.parser), on a branch of its own (eng-1-parser), off the
 * project's default branch, which its pull request goes into. Coders run
 * real commands, on this machine: use a sandbox.
 */
import { execFile } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { promisify } from 'node:util';
import * as restate from '@restatedev/restate-sdk';

const exec = promisify(execFile);

const git = async (cwd: string, ...args: string[]) => (await exec('git', args, { cwd })).stdout.trim();

const has = (cwd: string, ref: string) =>
  git(cwd, 'rev-parse', '--verify', '--quiet', ref).then(
    () => true,
    () => false,
  );

/**
 * GitHub's credentials, GITHUB_TOKEN, as git asks for them: read when it
 * pushes, so the token is in no URL, config or error message. The helpers
 * before it (your machine's, such as its keychain) are dropped: they would
 * answer, or ask you, first.
 */
const helper = '!f() { echo username=x-access-token; echo "password=$GITHUB_TOKEN"; }; f';
const credentials = ['-c', 'credential.helper=', '-c', `credential.helper=${helper}`];

/** A coder's key as its issue and task (`ENG-1.parser`). */
function taskOf(key: string) {
  const [issue = '', task = '', ...rest] = key.split('.');
  if (!/^[\w-]+$/.test(issue) || !/^[\w-]+$/.test(task) || rest.length > 0)
    throw new restate.TerminalError(`${key} is not an issue's task, such as ENG-1.parser`);
  return { issue, task };
}

/** The coder `key`'s branch: ENG-1.parser is on eng-1-parser. */
function branchOf(key: string) {
  const { issue, task } = taskOf(key);
  return `${issue.toLowerCase()}-${task}`;
}

/** The coder whose branch `branch` is (eng-1-parser is ENG-1.parser's), or undefined if none's. */
export function coderOfBranch(branch: string) {
  const [, issue, task] = /^([a-z][a-z0-9]*-\d+)-([a-z0-9-]+)$/.exec(branch) ?? [];
  return issue && task ? `${issue.toUpperCase()}.${task}` : undefined;
}

/**
 * The clone of the coder `key`, in `root`: made the first time, on its branch.
 * Each step is done once, so an attempt that failed half way is finished by the next.
 */
export async function clone(root: string, remote: string, key: string) {
  const { task } = taskOf(key);
  const dir = resolve(root, key);
  const branch = branchOf(key);
  if (await has(dir, `refs/heads/${branch}`)) return dir;

  if (!existsSync(join(dir, '.git'))) {
    await rm(dir, { recursive: true, force: true });
    await mkdir(root, { recursive: true });
    await git(root, ...credentials, 'clone', '--quiet', remote, dir);
  }
  await git(dir, 'config', '--replace-all', 'credential.helper', '');
  await git(dir, 'config', '--add', 'credential.helper', helper);
  await git(dir, 'config', 'user.name', `🛠️ Coder · ${task}`); // each coder's commits are its own, signed as its comments are
  await git(dir, 'config', 'user.email', 'factory@restate.localhost');
  await git(dir, 'config', 'push.autoSetupRemote', 'true'); // a plain `git push` pushes the coder's branch

  // The coder's branch: the remote's, if a run before pushed it, or else a new one off the default branch.
  await git(dir, 'fetch', '--quiet', 'origin');
  const from = (await has(dir, `origin/${branch}`)) ? `origin/${branch}` : 'origin/HEAD';
  await git(dir, 'checkout', '--quiet', '--no-track', '-b', branch, from);
  return dir;
}

/**
 * What the coder `key` pushed, as its pull request has it, read in its clone
 * in `root`: its changes against the default branch, and its files.
 */
export function pushed(root: string, key: string) {
  const dir = resolve(root, key);
  const branch = `origin/${branchOf(key)}`;
  return {
    changes: () => git(dir, 'diff', '--stat', '--patch', `origin/HEAD...${branch}`),
    file: (path: string) => git(dir, 'show', `${branch}:${path}`),
  };
}

/**
 * The project's repository, `remote`, with a first commit, a README, if it has
 * none: a new repository on GitHub is empty, and its coders' branches start
 * off its default branch.
 */
export async function project(remote: string) {
  if (await git(tmpdir(), ...credentials, 'ls-remote', '--heads', remote)) return remote;
  const first = await mkdtemp(join(tmpdir(), 'factory-'));
  await git(first, 'init', '--quiet', '--initial-branch', 'main');
  await writeFile(join(first, 'README.md'), '# A demo project\n\nBuilt by the factory.\n');
  await git(first, 'add', 'README.md');
  await git(
    first,
    '-c',
    'user.name=Restate factory',
    '-c',
    'user.email=factory@restate.localhost',
    'commit',
    '-qm',
    'A README',
  );
  await git(first, ...credentials, 'push', '--quiet', remote, 'main');
  await rm(first, { recursive: true, force: true });
  return remote;
}

/** A git repository in `root`, standing in for GitHub's (`origin.git`): made the first time. */
export async function standInRemote(root: string) {
  const remote = resolve(root, 'origin.git');
  if (existsSync(remote)) return remote;
  await mkdir(remote, { recursive: true });
  await git(remote, 'init', '--quiet', '--bare', '--initial-branch', 'main');
  return remote;
}
