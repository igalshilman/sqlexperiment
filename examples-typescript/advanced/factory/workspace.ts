/**
 * Coding tools for an agent whose framework has none of its own (the DeepSeek
 * Harness): list, read and write files, run commands, and commit, in a
 * directory per issue, which several agents work in at once. They run real
 * commands, on this machine: use a sandbox.
 */
import { execFile } from 'node:child_process';
import { mkdir, readdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, relative, resolve } from 'node:path';
import { promisify } from 'node:util';
import * as restate from '@restatedev/restate-sdk';
import { tool } from 'restate-agents';
import { z } from 'zod';

const exec = promisify(execFile);

/** A path of the directory: none outside it. */
const pathIn = (dir: string, path: string) => {
  const full = resolve(dir, path);
  if (relative(dir, full).startsWith('..')) throw new restate.TerminalError(`${path} is outside the project`);
  return full;
};

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

/**
 * The tools, in a directory of `root` per issue: a coder's key is its
 * issue's, then its task (`ENG-1.parser`), as subagents are keyed.
 */
export function codingTools(root: string) {
  /** The issue's directory, made if it is not there yet. */
  const directory = async (key: string) => {
    const dir = resolve(root, key.split('.')[0]!);
    await mkdir(dir, { recursive: true });
    return dir;
  };

  const listFiles = tool({
    name: 'list_files',
    description: "The project's files, but for .git.",
    input: z.object({}),
    run: async (_, { ctx }) => {
      const files = await readdir(await directory(ctx.key), { recursive: true });
      return files.filter((f) => !f.startsWith('.git')).join('\n') || '(no files yet)';
    },
  });

  const readText = tool({
    name: 'read_file',
    description: "A file's content.",
    input: z.object({ path: z.string().describe('The path, in the project') }),
    run: async ({ path }, { ctx }) =>
      readFile(pathIn(await directory(ctx.key), path), 'utf8').catch(() => {
        throw new restate.TerminalError(`No file ${path}`);
      }),
  });

  const writeText = tool({
    name: 'write_file',
    description: 'Write a file, in place of what it had.',
    input: z.object({ path: z.string().describe('The path, in the project'), content: z.string() }),
    run: async ({ path, content }, { ctx }) => {
      const file = pathIn(await directory(ctx.key), path);
      await mkdir(dirname(file), { recursive: true });
      await writeFile(file, content);
      return `wrote ${path}`;
    },
  });

  const bash = tool({
    name: 'bash',
    description: "Run a command in the project's directory (tests, say): its exit code and output.",
    input: z.object({ command: z.string() }),
    run: async ({ command }, { ctx }) => run('bash', ['-c', command], await directory(ctx.key)),
  });

  const commit = tool({
    name: 'commit',
    description: 'Commit files of the project, with a message: only these files, whatever others changed.',
    input: z.object({ files: z.array(z.string()).describe('The paths, in the project'), message: z.string() }),
    run: async ({ files, message }, { ctx }) => {
      const dir = await directory(ctx.key);
      for (const file of files) pathIn(dir, file);
      return oneAtATime(dir, async () => {
        await run('git', ['init', '--quiet'], dir);
        await run('git', ['add', '--', ...files], dir);
        // Each commit is its coder's: `coder/ENG-1.plan.history`, say.
        const author = ['-c', `user.name=coder/${ctx.key}`, '-c', 'user.email=coder@factory.localhost'];
        return run('git', [...author, 'commit', '--quiet', '--message', message, '--', ...files], dir);
      });
    },
  });

  return [listFiles, readText, writeText, bash, commit];
}
