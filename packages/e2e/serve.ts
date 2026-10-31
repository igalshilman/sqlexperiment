// Examples in one endpoint: every one, or the groups named on the command
// line (ledger, shop, agent, langgraph, claude, deepseek, opencode), such as
// `node serve.ts ledger shop`; with the agents' `signals` object.
import * as restate from '@restatedev/restate-sdk';
import { signals } from 'restate-agents';

type Services = restate.ServeOptions['services'];

const groups: Record<string, () => Promise<Services>> = {
  ledger: async () => [(await import('restate-sessions/test/ledger')).ledger],
  shop: async () => {
    const { warehouse, orders } = await import('restate-sessions/test/shop');
    return [warehouse, orders];
  },
  agent: async () => [(await import('restate-agents/test/pi')).agent],
  langgraph: async () => [(await import('restate-agents/test/langgraph')).agent],
  claude: async () => [(await import('restate-agents/test/claude')).agent],
  deepseek: async () => [(await import('restate-agents/test/deepseek')).agent],
  opencode: async () => [(await import('restate-agents/test/opencode')).agent],
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
await restate.serve({ services: [...services, signals], port: Number(process.env.PORT ?? 9080) });
