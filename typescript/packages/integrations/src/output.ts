// An agent's answer as data, with `output` (every framework's option): a zod
// object schema. The agent hands its answer in with a tool, `submit_answer`,
// whose input is the schema, so the model's call is checked against it; the
// prompt then answers with that value, not the model's text.
//
//   const planner = claude.agent({ name: 'planner', output: Plan });   // chat answers with a Plan
//
// The value is kept in the agent's database, by prompt, with its
// conversation: a retried attempt finds what it handed in. A model that ends
// its turn without handing an answer in is asked once more, then the prompt
// fails (a terminal error).
import * as restate from 'restate-fns';
import { Schema } from 'restate-fns/internal';
import type { z } from 'zod';
import { tool, type Tool } from './tools.ts';

/** What a prompt answers: its text, or with `output`, the value the agent handed in. */
export type Answer<O> = O extends z.ZodObject ? z.infer<O> : string;

const TABLE = 'restate_agent_outputs';
const SCHEMA = new Schema('restate-agents.output', [
  `CREATE TABLE ${TABLE} (prompt TEXT PRIMARY KEY, value TEXT NOT NULL)`,
]);

/** The tool's name, which the reminder names too. */
const SUBMIT = 'submit_answer';

/** What the agent is asked when it ended its turn without an answer. */
const REMIND = `You did not hand in your answer: call \`${SUBMIT}\` with it now.`;

/** The prompts with an output each invocation made so far, by its context. */
const outputs = new WeakMap<object, number>();

/**
 * The key of the next prompt with an output of the invocation of `ctx`: a
 * retried attempt makes them in the same order, so each finds its own value.
 */
const nextKey = (ctx: restate.ObjectContext) => {
  const n = (outputs.get(ctx) ?? 0) + 1;
  outputs.set(ctx, n);
  return `${ctx.request().id}:${n}`;
};

/** The value handed in for prompt `key`, if any. */
async function handedIn(db: restate.SqlSession, key: string): Promise<unknown> {
  await SCHEMA.ensure(db);
  const [row] = await db.query(`SELECT value FROM ${TABLE} WHERE prompt = ?`, [key]);
  return row ? JSON.parse(String(row[0])) : undefined;
}

/**
 * Answer `text` with a value of `output`: `prompt` (the framework's) asks
 * the agent with the tool that hands it in beside its own, and again, once,
 * if it did not.
 */
export async function answerWith<O extends z.ZodObject>(
  ctx: restate.ObjectContext,
  text: string,
  output: O,
  prompt: (text: string, tools: Tool[]) => Promise<string>,
): Promise<z.infer<O>> {
  const key = nextKey(ctx);
  const submit = tool({
    name: SUBMIT,
    description: 'Hand in your answer: what the one who asked gets. Hand it in again to change it.',
    input: output,
    run: async (value) => {
      await SCHEMA.ensure(ctx.db);
      await ctx.db.execute(`INSERT OR REPLACE INTO ${TABLE} VALUES (?, ?)`, [key, JSON.stringify(value)]);
      return 'Handed in.';
    },
  });
  await prompt(text, [submit]);
  let value = await handedIn(ctx.db, key);
  if (value === undefined) {
    await prompt(REMIND, [submit]);
    value = await handedIn(ctx.db, key);
  }
  if (value === undefined) throw new restate.TerminalError(`The agent handed in no answer (with ${SUBMIT})`);
  return output.parse(value);
}

/**
 * `prompt` (a framework's, of `options`), answering with a value of
 * `options.output` if it has one, or else with the agent's text.
 */
export const withOutput = <T extends { tools?: unknown[]; output?: z.ZodObject }>(
  ctx: restate.ObjectContext,
  text: string,
  options: T,
  prompt: (text: string, options: T) => Promise<string>,
): Promise<Answer<T['output']>> =>
  (options.output
    ? answerWith(ctx, text, options.output, (t, tools) =>
        prompt(t, { ...options, tools: [...(options.tools ?? []), ...tools] }),
      )
    : prompt(text, options)) as Promise<Answer<T['output']>>;
