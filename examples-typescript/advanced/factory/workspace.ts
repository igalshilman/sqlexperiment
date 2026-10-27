/**
 * The coders' workspace: a directory per issue, which the issue's coders
 * work in at once (OpenCode's own tools: read, write, edit, bash...), and
 * `commit`, which commits one coder's files, one commit at a time. Coders run
 * real commands, on this machine: use a sandbox.
 */
import { execFile } from 'node:child_process';
import { relative, resolve } from 'node:path';
import { promisify } from 'node:util';
import * as restate from '@restatedev/restate-sdk';
import { tool } from 'restate-agents';
import { z } from 'zod';

const exec = promisify(execFile);

/** The directory of a coder's issue, in `root`: its key is the issue's, then its task (`ENG-1.parser`). */
export function issueDirectory(root: string, key: string) {
  const issue = key.split('.')[0]!;
  if (!/^[\w-]+$/.test(issue)) throw new restate.TerminalError(`${key} is not an issue's task, such as ENG-1.parser`);
  return resolve(root, issue);
}

/** A command's exit code and output (its last 10,000 characters). */
async function run(file: string, args: string[], cwd: string) {
  const { code, output } = await exec(file, args, { cwd, timeout: 120_000 }).then(
    ({ stdout, stderr }) => ({ code: 0, output: stdout + stderr }),
    (error: { code?: number; stdout?: string; stderr?: string; message: string }) => ({
      code: error.code ?? 1,
      output: (error.stdout ?? '') + (error.stderr ?? error.message),
    }),
  );
  return `exit ${code}\n${output.slice(-10_000)}`;
}

/** Each directory's commits, one after another: two at once would race for git's index. */
const commits = new Map<string, Promise<unknown>>();

const oneAtATime = <T>(dir: string, work: () => Promise<T>): Promise<T> => {
  const done = (commits.get(dir) ?? Promise.resolve()).then(work);
  commits.set(
    dir,
    done.catch(() => undefined),
  );
  return done;
};

/** `commit`: a coder's files, in its issue's directory of `root`, as its own commit. */
export const commitTool = (root: string) =>
  tool({
    name: 'commit',
    description: 'Commit files of the project, with a message: only these files, whatever others changed.',
    input: z.object({ files: z.array(z.string()).describe('The paths, in the project'), message: z.string() }),
    run: async ({ files, message }, { ctx }) => {
      const dir = issueDirectory(root, ctx.key);
      for (const file of files) {
        if (relative(dir, resolve(dir, file)).startsWith('..'))
          throw new restate.TerminalError(`${file} is outside the project`);
      }
      return oneAtATime(dir, async () => {
        await run('git', ['init', '--quiet'], dir);
        await run('git', ['add', '--', ...files], dir);
        // Each commit is its coder's: `coder/ENG-1.history`, say.
        const author = ['-c', `user.name=coder/${ctx.key}`, '-c', 'user.email=coder@factory.localhost'];
        return run('git', [...author, 'commit', '--quiet', '--message', message, '--', ...files], dir);
      });
    },
  });
