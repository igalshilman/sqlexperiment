// Examples in one endpoint: every one, or the groups named on the command
// line (ledger, shop, agent, langgraph, claude, deepseek), such as `node serve.mjs ledger shop`.
import * as restate from '@restatedev/restate-sdk';

const groups = {
  ledger: async () => [(await import('./node/ledger.mjs')).ledger],
  shop: async () => {
    const { warehouse, orders } = await import('./node/shop.mjs');
    return [warehouse, orders];
  },
  agent: async () => [(await import('./pi/agent.mjs')).agent],
  langgraph: async () => [(await import('./langgraph/agent.mjs')).agent],
  claude: async () => [(await import('./claude/agent.mjs')).agent],
  deepseek: async () => [(await import('./deepseek/agent.mjs')).agent],
};
const names = process.argv.length > 2 ? process.argv.slice(2) : Object.keys(groups);
const services = (await Promise.all(names.map((name) => groups[name]()))).flat();
restate.serve({ services, port: Number(process.env.PORT ?? 9080) });
