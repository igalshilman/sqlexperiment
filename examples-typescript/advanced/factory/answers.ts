/**
 * The agents' answers, structured: each is an agent's `output`, which its
 * model replies with, as JSON of the schema (checked against it), and its
 * `chat` answers with.
 */
import { z } from 'zod';

/** The planner's: a plan the parent can act on, part by part. */

export const Plan = z.object({
  summary: z.string().describe('What will be built, and how its parts fit together: a short paragraph, for people'),
  parts: z
    .array(
      z.object({
        name: z
          .string()
          .regex(/^[a-z0-9-]+$/)
          .describe('A short name, such as "parser"'),
        title: z.string().describe('The part, in a line: the title of its sub-issue'),
        description: z
          .string()
          .describe(
            "The part, for the people following the issue: two or three plain sentences, its sub-issue's description",
          ),
        files: z.array(z.string()).describe('The files the part owns: no other part changes them'),
        task: z.string().describe('What to build, in full: its coder sees nothing else'),
      }),
    )
    .min(1)
    .describe('Parts that can be built at the same time'),
});

export type Plan = z.infer<typeof Plan>;

/** The reviewer's: a pull request approved, or the changes to make first. */
export const Review = z.object({
  approved: z.boolean().describe('Whether it is ready to merge as it is'),
  summary: z.string().describe('The review, in a few sentences: what is good, and what is not'),
  changes: z.array(z.string()).describe('What to change before it is ready, each in a sentence; none if approved'),
});

/** A coder's: its part's pull request, and whether the reviewer approved it. */
export const Built = z.object({
  pullRequest: z.string().describe("The pull request's link, as create_pull_request answered"),
  approved: z.boolean().describe('Whether the reviewer approved it'),
  summary: z
    .string()
    .describe('What was built, and its tests, in a sentence or two; and what the reviewer still asks for, if anything'),
});
