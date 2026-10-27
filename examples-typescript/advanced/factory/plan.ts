/**
 * The planner's answer, structured: a plan the parent can act on, part by
 * part. It is the planner's `output`: the model hands it in with a tool whose
 * input is this schema, so its call is checked against it, and the planner's
 * `chat` answers with it.
 */
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
