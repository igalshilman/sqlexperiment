/**
 * The coders' workspace, in git: a clone of the project per issue
 * (WORKSPACE/ENG-1), and a worktree of it per coder (WORKSPACE/ENG-1.parser),
 * on a branch of its own (eng-1-parser), off the issue's branch (eng-1), which
 * its pull request goes into. The `integrate` coder's worktree is on the
 * issue's branch itself: it merges the parts there. Coders run real commands,
 * on this machine: use a sandbox.
 */
import { execFile } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdir, mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { promisify } from 'node:util';
import * as restate from '@restatedev/restate-sdk';

const exec = promisify(execFile);

const git = async (cwd: string, ...args: string[]) => (await exec('git', args, { cwd })).stdout.trim();

/** The issue's branch: its identifier, in lower case (ENG-1: eng-1). */
export const issueBranch = (issue: string) => issue.toLowerCase();

/** A coder's key as its issue and task (`ENG-1.parser`). */
function taskOf(key: string) {
  const [issue = '', task = '', ...rest] = key.split('.');
  if (!/^[\w-]+$/.test(issue) || !/^[\w-]+$/.test(task) || rest.length > 0)
    throw new restate.TerminalError(`${key} is not an issue's task, such as ENG-1.parser`);
  return { issue, task };
}

/** Each issue's git work, one step after another: its coders start at once. */
const steps = new Map<string, Promise<unknown>>();

const oneAtATime = <T>(issue: string, work: () => Promise<T>): Promise<T> => {
  const done = (steps.get(issue) ?? Promise.resolve()).then(work);
  steps.set(
    issue,
    done.catch(() => undefined),
  );
  return done;
};

/**
 * The worktree of the coder `key`, in `root`, made the first time (and the
 * issue's clone of `remote` with it, and its branch, on the remote too).
 */
export async function worktree(root: string, remote: string, key: string) {
  const { issue, task } = taskOf(key);
  const dir = resolve(root, key);
  return oneAtATime(issue, async () => {
    if (existsSync(dir)) return dir;
    const clone = resolve(root, issue);
    const base = issueBranch(issue);
    if (!existsSync(join(clone, '.git'))) {
      await mkdir(root, { recursive: true });
      await git(root, 'clone', '--quiet', remote, clone);
      await git(clone, 'config', 'user.name', 'Restate factory');
      await git(clone, 'config', 'user.email', 'factory@restate.localhost');
      await git(clone, 'config', 'extensions.worktreeConfig', 'true');
      // The issue's branch: the remote's, if a run before made it, or else a new one off its default branch.
      if (await git(clone, 'ls-remote', '--heads', 'origin', base)) await git(clone, 'branch', base, `origin/${base}`);
      else {
        await git(clone, 'branch', '--no-track', base, 'origin/HEAD');
        await git(clone, 'push', '--quiet', '-u', 'origin', base);
      }
    }
    if (task === 'integrate') await git(clone, 'worktree', 'add', '--quiet', dir, base);
    else await git(clone, 'worktree', 'add', '--quiet', '-b', `${base}-${task}`, dir, base);
    // Each coder's commits are its own.
    await git(dir, 'config', '--worktree', 'user.name', `Coder · ${task}`);
    return dir;
  });
}

/** A git repository in `root`, standing in for GitHub's (`origin.git`): made the first time, with a README. */
export async function standInRemote(root: string) {
  const remote = resolve(root, 'origin.git');
  if (existsSync(remote)) return remote;
  await mkdir(remote, { recursive: true });
  await git(remote, 'init', '--quiet', '--bare', '--initial-branch', 'main');
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
  await git(first, 'push', '--quiet', remote, 'main');
  return remote;
}
