/**
 * 07 · Once per invocation: steps
 *
 * Some work must not run twice: drawing a random number, charging a card,
 * asking a model. `ctx.step(name, body)` runs `body` once for the
 * invocation, as a transaction (05) whose result commits with its SQL. A
 * retry that reaches the step gets the stored result, and skips `body`.
 *
 *   node src/07_step.ts                       # and register it (see 01)
 *   curl localhost:8080/lottery/alice/draw    # "You drew 42": this process logs one draw, though there were two attempts
 *   curl localhost:8080/lottery/alice/draws   # [42]
 */
import * as restate from 'restate-objects';

const failed = new Set<string>();

/** Fail the first attempt of each invocation, as a crash would: Restate retries it. */
function failFirstAttempt(ctx: restate.ObjectContext) {
  if (failed.has(ctx.request().id)) return;
  failed.add(ctx.request().id);
  throw new Error('A crash, after the draw committed');
}

const lottery = restate.object({
  name: 'lottery',
  schema: ['CREATE TABLE draws (number INTEGER NOT NULL)'],
  handlers: {
    draw: async (ctx: restate.ObjectContext) => {
      const number = await ctx.step('draw', async ({ db }) => {
        const drawn = Math.ceil(Math.random() * 100);
        console.log(`Drew ${drawn}`);
        await db.execute('INSERT INTO draws (number) VALUES (?)', [drawn]);
        return drawn;
      });
      failFirstAttempt(ctx);
      return `You drew ${number}`;
    },

    draws: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext) => {
      const rows = await ctx.db.query('SELECT number FROM draws ORDER BY rowid');
      return rows.map(([number]) => number);
    }),
  },
});

await restate.serve({ services: [lottery], port: 9080 });
