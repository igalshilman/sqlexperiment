/**
 * The pair's tools, beside pi's coding tools (read, write, edit and bash, in
 * the project's directory): the plan, a table of the project's database that
 * the pair keeps up to date as it works.
 */
import { tool } from 'restate-agents';
import type { SqlDatabase } from 'restate-fns';
import { z } from 'zod';

/** The plan's table, the project's own, beside the agent's: the pair's schema. */
export const PLAN = ['CREATE TABLE plan (n INTEGER PRIMARY KEY, task TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0)'];

/** A task of the plan. */
export interface Task {
  n: number;
  task: string;
  done: boolean;
}

/** The plan: each task's number, text, and whether it is done. */
export async function tasksOf(db: SqlDatabase): Promise<Task[]> {
  const rows = await db.query('SELECT n, task, done FROM plan ORDER BY n');
  return rows.map(([n, task, done]) => ({ n: Number(n), task: String(task), done: Boolean(done) }));
}

export const plan = tool({
  name: 'plan',
  description: 'Set the plan of the work you were asked for: its tasks, in order, in place of the plan before.',
  input: z.object({ tasks: z.array(z.string()) }),
  run: async ({ tasks }, { ctx }) => {
    await ctx.db.transaction(async (tx) => {
      await tx.execute('DELETE FROM plan');
      for (const task of tasks) await tx.execute('INSERT INTO plan (task) VALUES (?)', [task]);
    });
    return tasksOf(ctx.db);
  },
});

export const done = tool({
  name: 'done',
  description: "Mark the plan's task `n` (from 1) done.",
  input: z.object({ n: z.number().int() }),
  run: async ({ n }, { ctx }) => {
    await ctx.db.execute('UPDATE plan SET done = 1 WHERE n = ?', [n]);
    return tasksOf(ctx.db);
  },
});
