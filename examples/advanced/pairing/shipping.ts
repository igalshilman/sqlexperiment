/**
 * Shipping a change, as a Restate workflow: its checks run in CI, then its
 * pull request opens, once per change (the workflow's key), however often it
 * is asked. CI and the repository are stand-ins: a wait, and a log.
 */
import * as restate from '@restatedev/restate-sdk';
import { z } from 'zod';

export const Change = z.object({
  title: z.string().describe("The pull request's title"),
  summary: z.string().describe('What changed and why, for the people who review it'),
  diff: z.string().describe('The change, as `git diff` shows it'),
});

export const shipping = restate.workflow({
  name: 'shipping',
  handlers: {
    /** Ship a change that the person approved: CI checks it, then its pull request opens. Its URL. */
    run: async (ctx: restate.WorkflowContext, change: z.infer<typeof Change>) => {
      await ctx.sleep({ seconds: 5 }); // a stand-in for your CI
      await ctx.run('open the pull request', () =>
        console.log(`${change.title}\n\n${change.summary}\n\n${change.diff}`),
      );
      return `https://example.com/pulls/${ctx.key}`;
    },
  },
});
