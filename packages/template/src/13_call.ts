/**
 * 13 · Your objects as tools
 *
 * `call(object, handler)` makes a handler of yours a tool: the model picks
 * the key (here, a product) and the agent calls it through Restate, once,
 * however often the agent's handler is retried.
 *
 *   node src/13_call.ts   # and register it (see 01)
 *   curl localhost:8080/inventory/apples/restock --json 12
 *   curl localhost:8080/clerk/alice/chat -d 'Do we have apples? And pears?'
 */
import * as restate from 'restate-objects';
import { call, serve } from 'restate-agents';
import * as pi from 'restate-agents/pi';

const inventory = restate.object({
  name: 'inventory',
  schema: ['CREATE TABLE stock (count INTEGER NOT NULL)', 'INSERT INTO stock VALUES (0)'],
  handlers: {
    restock: async (ctx: restate.ObjectContext, count: number) => {
      await ctx.db.execute('UPDATE stock SET count = count + ?', [count]);
    },

    count: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext) => {
      const [count] = (await ctx.db.values('SELECT count FROM stock')).map(([n]) => n);
      return count;
    }),
  },
});

const clerk = pi.agent({
  name: 'clerk',
  tools: [call(inventory, 'count', { description: 'How many units of a product are in stock.' })],
});

await serve(inventory, clerk);
