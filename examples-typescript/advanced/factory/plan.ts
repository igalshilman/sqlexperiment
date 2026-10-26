/**
 * The planner's answer, structured: a plan the parent can act on, part by
 * part. The planner submits it with a tool, `submit_plan`, whose input is the
 * plan's schema: the model's tool call is checked against it, and the plan
 * is kept in the planner's database, one per invocation, for the `plan`
 * handler to answer with.
 */
import * as restate from '@restatedev/restate-sdk';
import { tool } from 'restate-agents';
import type { SqlDatabase } from 'restate-fns';
import { z } from 'zod';

export const Plan = z.object({
  summary: z.string().describe('What will be built, and how its parts fit together'),
  parts: z
    .array(
      z.object({
        name: z
          .string()
          .regex(/^[a-z0-9-]+$/)
          .describe('A short name, such as "parser"'),
        title: z.string().describe('The part, in a line: the title of its sub-issue'),
        files: z.array(z.string()).describe('The files the part owns: no other part changes them'),
        task: z.string().describe('What to build, in full: its coder sees nothing else'),
      }),
    )
    .min(1)
    .describe('Parts that can be built at the same time'),
});

export type Plan = z.infer<typeof Plan>;

/** The plans, the planner's table beside its conversation: the plan of each invocation. */
export const PLANS = ['CREATE TABLE plans (invocation TEXT PRIMARY KEY, plan TEXT NOT NULL)'];

export const submitPlan = tool({
  name: 'submit_plan',
  description: 'Submit your plan: the answer to the one who asked for it. Submit again to change it.',
  input: Plan,
  run: async (plan, { ctx }) => {
    await ctx.db.execute('INSERT OR REPLACE INTO plans VALUES (?, ?)', [ctx.request().id, JSON.stringify(plan)]);
    return 'submitted';
  },
});

/** The plan this invocation submitted. */
export async function planOf(ctx: { db: SqlDatabase; request(): { id: string } }): Promise<Plan> {
  const [row] = await ctx.db.query('SELECT plan FROM plans WHERE invocation = ?', [ctx.request().id]);
  if (!row) throw new restate.TerminalError('The planner submitted no plan');
  return Plan.parse(JSON.parse(String(row[0])));
}
