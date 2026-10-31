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
import * as restate from 'restate-sessions';
import { call, serve } from 'restate-agents';
import * as pi from 'restate-agents/pi';

const inventory = restate.sessionObject({
  name: 'inventory',
  schema: ['CREATE TABLE stock (count INTEGER NOT NULL)', 'INSERT INTO stock VALUES (0)'],
  handlers: {
    restock: async (ctx, count: number) => {
      await ctx.db.execute('UPDATE stock SET count = count + ?', [count]);
    },
  },
  views: {
    count: async (ctx) => {
      const stock = await ctx.db.get<{ count: number }>('SELECT count FROM stock');
      return stock?.count;
    },
  },
});

const clerk = pi.agent({
  name: 'clerk',
  tools: [call(inventory, 'count', { description: 'How many units of a product are in stock.' })],
});

await serve(inventory, clerk);
