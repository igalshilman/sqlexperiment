// A coding sub-agent: OpenCode 2.0 (../opencode) in a Coder object per coding
// task, keyed `<tenant>.<user>.<task>` (`acme.alice.fizzbuzz`). The Assistant
// (agent.ts) hands it a task with its `code` tool. Each task is another
// object, with its own OpenCode session in its own database and its own
// directory: tasks of different names run at once, and where each object
// runs in a machine of its own, in different machines. The same name is the
// same object, which continues where it left off.
//
// OpenCode runs embedded, with its database in the object's, and its own
// tools (read, write, edit, bash, glob, grep...) on the local machine, in the
// object's directory, `$WORKSPACE/Coder/<key>` (a temporary directory without
// WORKSPACE). A crash resumes the turn, and a tool call it cut off is
// reported as interrupted, not run again.
//
// Environment: OPENCODE_MODEL, or else PI_MODEL, as provider/model-id (such as
// openai/gpt-5.4-mini, with OPENAI_API_KEY): the same model as the Assistant,
// which OpenCode's own providers serve. A real model only: without one, a
// task fails. WORKSPACE, PORT.
import * as restate from '@restatedev/restate-sdk';
import { mkdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { z } from 'zod';
import { serveIfMain } from '../faults.ts';
import { withOpenCode } from '../opencode/with-opencode.ts';

/**
 * The object's own directory in WORKSPACE, if set: `<service>/<key>`, the
 * key's characters other than letters, digits, `.`, `_` and `-` as `_`.
 */
export function workspaceOf(service: string, key: string, root = process.env.WORKSPACE): string | undefined {
  if (!root) return undefined;
  const name = key.replace(/[^\w.-]/g, '_');
  return join(root, service, /^\.*$/.test(name) ? `_${name}` : name);
}

/** The model, OPENCODE_MODEL or else PI_MODEL, as OpenCode names it. */
function modelOf(name = process.env.OPENCODE_MODEL ?? process.env.PI_MODEL) {
  const [providerID = '', ...id] = (name ?? '').split('/');
  if (!providerID || id.length === 0) return undefined;
  return { providerID, id: id.join('/') };
}

const Task = z.object({ task: z.string().describe('The coding task, in full') });
const Answer = z.object({ answer: z.string().describe("The coder's report") });

/** Do `task` in the object's workspace, once per invocation. */
export const code = async (ctx: restate.SqlObjectContext, { task }: z.infer<typeof Task>) => {
  const model = modelOf();
  if (!model) throw new restate.TerminalError('Coder needs a real model: set PI_MODEL (or OPENCODE_MODEL)');
  const dir = workspaceOf('Coder', ctx.key) ?? workspaceOf('Coder', ctx.key, join(tmpdir(), 'coder'))!;
  await mkdir(dir, { recursive: true });
  // One prompt per invocation: a retried attempt resumes the same one.
  const id = ctx.request().id.replace(/[^\w-]/g, '_');
  return withOpenCode(ctx, { model, directory: dir }, async (opencode) => ({
    answer: await opencode.prompt(id, task),
  }));
};

export const coder = restate.object({
  name: 'Coder',
  handlers: {
    code: restate.createObjectHandler(
      {
        description: [
          'Do a coding task with OpenCode, in the workspace of the key `<tenant>.<user>.<task>` (such as `acme.alice.fizzbuzz`).',
          'The same key continues the same session and workspace.',
        ].join('\n'),
        input: restate.serde.schema(Task),
        output: restate.serde.schema(Answer),
        journalReplay: false,
      },
      code,
    ),
  },
  // Hide the SQL storage from the UI's journal view, as the Assistant does.
  metadata: {
    'dev.restate.tooling.journal.hidden.names': 'sync,_restate_sql_sends',
    'dev.restate.tooling.journal.hidden.name.prefix': 'v13/',
  },
  options: {
    ...restate.sqlOptions,
    // A task is long work, and each attempt resumes from storage.
    retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
    journalRetention: { hours: 1 },
  },
});

serveIfMain(import.meta, [coder]);
