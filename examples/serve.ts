// Examples in one endpoint: every one, or the groups named on the command
// line (ledger, shop, agent, langgraph, claude, deepseek, opencode), such as
// `node serve.ts ledger shop`.
import * as restate from '@restatedev/restate-sdk';

type Services = restate.ServeOptions['services'];

const groups: Record<string, () => Promise<Services>> = {
  ledger: async () => [(await import('./node/ledger.ts')).ledger],
  shop: async () => {
    const { warehouse, orders } = await import('./node/shop.ts');
    return [warehouse, orders];
  },
  agent: async () => [(await import('./pi/agent.ts')).agent],
  langgraph: async () => [(await import('./langgraph/agent.ts')).agent],
  claude: async () => [(await import('./claude/agent.ts')).agent],
  deepseek: async () => [(await import('./deepseek/agent.ts')).agent],
  opencode: async () => [(await import('./opencode/agent.ts')).agent],
};
const names = process.argv.length > 2 ? process.argv.slice(2) : Object.keys(groups);
const services = (
  await Promise.all(
    names.map((name) => {
      const group = groups[name];
      if (!group) throw new Error(`no group ${name}: ${Object.keys(groups).join(', ')}`);
      return group();
    }),
  )
).flat();
await restate.serve({ services, port: Number(process.env.PORT ?? 9080) });
